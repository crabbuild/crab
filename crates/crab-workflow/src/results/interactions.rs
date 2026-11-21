use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{AxisRange, Chart, MAX_SERIES_POINTS, check_digest, digest, encode, invalid};
use crate::Result;

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(test, derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChartInteraction {
    pub key: String,
    pub field: String,
    pub label: String,
    pub domain: Option<AxisRange>,
}

pub(super) fn observe(bounds: &mut BTreeMap<String, AxisRange>, field: &str, value: &str) {
    let Some(value) = value.parse::<f64>().ok().filter(|value| value.is_finite()) else {
        return;
    };
    bounds
        .entry(field.into())
        .and_modify(|range| {
            range.lower = range.lower.min(value);
            range.upper = range.upper.max(value);
        })
        .or_insert(AxisRange {
            lower: value,
            upper: value,
        });
}

pub(super) fn augment(
    spec: &mut Value,
    bounds: &BTreeMap<String, AxisRange>,
    categories: &BTreeSet<String>,
) -> Result<Option<ChartInteraction>> {
    let x = spec["encoding"]["x"].clone();
    let Some(reference) = x["field"].as_str() else {
        return Ok(None);
    };
    let Some(field) = super::plot_specs::literal_field(reference) else {
        return Ok(None);
    };
    if x.get("aggregate").is_some()
        || x.get("bin").is_some()
        || x.get("timeUnit").is_some()
        || spec.get("transform").is_some()
    {
        return Ok(None);
    }
    if x["scale"]["type"]
        .as_str()
        .is_some_and(|kind| !["linear", "log", "sqrt", "pow", "symlog"].contains(&kind))
    {
        return Ok(None);
    }
    let domain = if x["type"] == "quantitative" {
        let range = if let Some(values) = x["scale"]["domain"].as_array() {
            if values.len() != 2 {
                return Ok(None);
            }
            values[0]
                .as_f64()
                .zip(values[1].as_f64())
                .map(|(lower, upper)| AxisRange { lower, upper })
        } else if x["scale"]["domain"].is_null() && x["scale"] != false {
            bounds.get(&field).copied()
        } else {
            None
        };
        let Some(range) = range.filter(|range| range.validate().is_ok()) else {
            return Ok(None);
        };
        if x["scale"]["type"] == "log" && range.lower <= 0.0 {
            return Ok(None);
        }
        Some(range)
    } else {
        if categories.len() > MAX_SERIES_POINTS {
            return Ok(None);
        }
        None
    };
    // Definition includes the authored label, scale and units. Equal field
    // names alone cannot establish compatible coordinates for linked views.
    let identity = if domain.is_some() {
        json!([x, domain])
    } else {
        json!([x, categories])
    };
    let interaction = ChartInteraction {
        key: digest(&encode(&identity)?),
        field: field.clone(),
        label: x["title"].as_str().unwrap_or(&field).into(),
        domain,
    };
    if spec["params"].is_null() {
        spec["params"] = json!([])
    }
    let parameters = spec["params"]
        .as_array_mut()
        .ok_or_else(|| invalid("plot params must be an array"))?;
    let reserved = [
        "crab_shared_x",
        "crab_domain",
        "crab_extent",
        "crab_hover",
        "crab_cursor",
    ];
    if parameters.iter().any(|parameter| {
        parameter["name"]
            .as_str()
            .is_some_and(|name| reserved.contains(&name))
    }) {
        return Err(invalid(
            "plot parameter conflicts with a reserved Results interaction name",
        ));
    }
    // Authored selections own their unit view. Repeating them across generated
    // layers duplicates Vega-Lite signals and prevents the chart from rendering.
    if parameters
        .iter()
        .any(|parameter| parameter.get("select").is_some())
    {
        return Ok(None);
    }
    parameters.push(json!({"name":"crab_shared_x", "value":null}));
    if let Some(range) = domain {
        parameters.push(json!({"name":"crab_domain", "value":null}));
        parameters.push(json!({"name":"crab_extent", "value":[range.lower,range.upper]}));
    }
    let mark = spec
        .as_object_mut()
        .and_then(|object| object.remove("mark"))
        .ok_or_else(|| invalid("line chart lacks a mark"))?;
    let mark = match mark {
        Value::String(kind) => json!({"type":kind,"clip":true}),
        Value::Object(mut mark) => {
            mark.insert("clip".into(), true.into());
            Value::Object(mark)
        }
        _ => return Err(invalid("invalid line mark")),
    };
    // Authored channels and configuration can use complete sample metadata.
    // Only simple encodings can replace two-dimensional hits with one per x.
    let scalar_cursor = spec.get("config").is_none()
        && spec["encoding"].as_object().is_some_and(|channels| {
            channels
                .keys()
                .all(|channel| matches!(channel.as_str(), "x" | "y" | "color" | "strokeDash"))
        });
    let mut transforms = Vec::new();
    if scalar_cursor {
        transforms.push(json!({"aggregate":[],"groupby":[reference]}));
    }
    if domain.is_some() {
        let field = serde_json::to_string(&field).map_err(crate::WorkflowError::ResultsJson)?;
        transforms.push(json!({"filter":format!("!isValid(crab_domain) || (datum[{field}] >= crab_domain[0] && datum[{field}] <= crab_domain[1])")}));
    }
    let mut point = json!({"mark":{"type":"point","opacity":0,"aria":false,"clip":true},
        "transform":transforms,
        "params":[{"name":"crab_hover","select":{"type":"point","fields":[field],"nearest":true,"on":"pointerover","clear":"pointerout"}}]});
    if scalar_cursor {
        point["encoding"] =
            json!({"y":{"value":0},"color":{"value":"transparent"},"strokeDash":{"value":[]}});
    }
    // Independent rule data must contain the inherited x field. Vega-Lite's
    // quantitative validation otherwise discards its empty datum before encoding.
    spec["layer"] = json!([
        {"mark":mark}, point,
        {"data":{"values":[{}]},"mark":{"type":"rule","color":"#888","aria":false,"clip":true},
         "transform":[{"calculate":"crab_shared_x","as":field},{"filter":"isValid(crab_shared_x)"}],
         "encoding":{"x":{"field":reference,"type":x["type"]},
            "y":{"value":0},"y2":{"value":{"expr":"height"}},
            "color":{"value":"#888"},"strokeDash":{"value":[]}}}
    ]);
    Ok(Some(interaction))
}

pub(super) fn validate_ranges(ranges: &BTreeMap<String, AxisRange>) -> Result<()> {
    if ranges.len() > 32 {
        return Err(invalid("choose at most 32 saved axis ranges"));
    }
    for (key, range) in ranges {
        check_digest(key)?;
        range.validate()?;
    }
    Ok(())
}

pub(super) fn apply_axes(charts: &mut [Chart], ranges: &BTreeMap<String, AxisRange>) -> Result<()> {
    validate_ranges(ranges)?;
    for chart in &mut *charts {
        if chart
            .interaction
            .as_ref()
            .is_some_and(|axis| axis.domain.is_some())
        {
            let parameter = chart
                .spec
                .as_mut()
                .and_then(|spec| spec["params"].as_array_mut())
                .and_then(|parameters| {
                    parameters
                        .iter_mut()
                        .find(|parameter| parameter["name"] == "crab_domain")
                })
                .ok_or_else(|| invalid("chart lacks its axis range parameter"))?;
            parameter["value"] = Value::Null;
        }
    }
    for (key, range) in ranges {
        let mut found = false;
        for chart in &mut *charts {
            let Some(axis) = &chart.interaction else {
                continue;
            };
            if &axis.key != key {
                continue;
            }
            let domain = axis
                .domain
                .ok_or_else(|| invalid("axis has no continuous range"))?;
            if range.lower < domain.lower || range.upper > domain.upper {
                return Err(invalid(
                    "axis range exceeds its observed or authored domain",
                ));
            }
            let parameter = chart
                .spec
                .as_mut()
                .and_then(|spec| spec["params"].as_array_mut())
                .and_then(|parameters| {
                    parameters
                        .iter_mut()
                        .find(|parameter| parameter["name"] == "crab_domain")
                })
                .ok_or_else(|| invalid("chart lacks its axis range parameter"))?;
            parameter["value"] = json!([range.lower, range.upper]);
            found = true;
        }
        if !found {
            return Err(invalid("saved range refers to an unavailable axis"));
        }
    }
    Ok(())
}

pub(super) fn verify_axes(charts: &[Chart], ranges: &BTreeMap<String, AxisRange>) -> Result<()> {
    for chart in charts {
        if let Some(axis) = &chart.interaction {
            check_digest(&axis.key)?;
            if axis.field.is_empty() || axis.field.len() > 4096 || axis.label.len() > 4096 {
                return Err(invalid("invalid chart interaction identity"));
            }
            if let Some(domain) = axis.domain {
                domain.validate()?;
            }
        }
    }
    let mut expected = charts.to_vec();
    apply_axes(&mut expected, ranges)?;
    if expected != charts {
        return Err(invalid(
            "frozen chart range differs from its report specification",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line() -> Value {
        json!({"mark":"line", "encoding":{"x":{"field":"step","type":"quantitative","title":"Epoch"}, "y":{"field":"value","type":"quantitative"}},
            "params":[{"name":"threshold","value":0.5}]})
    }
    fn bounds() -> BTreeMap<String, AxisRange> {
        BTreeMap::from([(
            "step".into(),
            AxisRange {
                lower: 0.0,
                upper: 10.0,
            },
        )])
    }
    #[test]
    fn linked_coordinates_require_equal_labels_scales_and_domains() {
        let original = augment(&mut line(), &bounds(), &BTreeSet::new())
            .unwrap()
            .unwrap();
        let mut variants = vec![line(), line(), line()];
        variants[0]["encoding"]["x"]["title"] = json!("Seconds");
        variants[1]["encoding"]["x"]["scale"] = json!({"zero":false});
        variants[2]["encoding"]["x"]["scale"] = json!({"domain":[1,9]});
        for mut spec in variants {
            let different = augment(&mut spec, &bounds(), &BTreeSet::new())
                .unwrap()
                .unwrap();
            assert_ne!(different.key, original.key);
        }
        assert_eq!(
            augment(&mut line(), &bounds(), &BTreeSet::new())
                .unwrap()
                .unwrap(),
            original
        );
    }
    #[test]
    fn authored_parameters_survive_and_reserved_names_fail_visibly() {
        let mut spec = line();
        augment(&mut spec, &bounds(), &BTreeSet::new()).unwrap();
        assert_eq!(spec["params"][0], json!({"name":"threshold","value":0.5}));
        let mut conflict = line();
        conflict["params"][0]["name"] = json!("crab_domain");
        assert!(augment(&mut conflict, &bounds(), &BTreeSet::new()).is_err());
    }
    #[test]
    fn transformed_coordinates_are_left_authored_without_unproven_linkage() {
        let mut spec = line();
        spec["transform"] = json!([{"filter":"datum.step > 3"}]);
        let authored = spec.clone();
        assert_eq!(
            augment(&mut spec, &bounds(), &BTreeSet::new()).unwrap(),
            None
        );
        assert_eq!(spec, authored);
    }
    #[test]
    fn frozen_ranges_refuse_unknown_out_of_bounds_and_tampered_parameters() {
        let mut spec = line();
        let interaction = augment(&mut spec, &bounds(), &BTreeSet::new())
            .unwrap()
            .unwrap();
        let chart = Chart {
            key: "loss".into(),
            title: "Loss".into(),
            state: super::super::Availability::Present,
            reason: None,
            spec: Some(spec),
            points: 2,
            displayed_points: 2,
            reduction: None,
            interaction: Some(interaction.clone()),
            images: Vec::new(),
        };
        let mut charts = vec![chart];
        for (key, range) in [
            (
                digest(b"unknown"),
                AxisRange {
                    lower: 1.0,
                    upper: 2.0,
                },
            ),
            (
                interaction.key.clone(),
                AxisRange {
                    lower: -1.0,
                    upper: 2.0,
                },
            ),
            (
                interaction.key.clone(),
                AxisRange {
                    lower: 2.0,
                    upper: 1.0,
                },
            ),
            (
                interaction.key.clone(),
                AxisRange {
                    lower: f64::NAN,
                    upper: 2.0,
                },
            ),
        ] {
            assert!(apply_axes(&mut charts.clone(), &BTreeMap::from([(key, range)])).is_err());
        }
        let ranges = BTreeMap::from([(
            interaction.key,
            AxisRange {
                lower: 2.0,
                upper: 6.0,
            },
        )]);
        apply_axes(&mut charts, &ranges).unwrap();
        verify_axes(&charts, &ranges).unwrap();
        assert!(verify_axes(&charts, &BTreeMap::new()).is_err());
    }
}
