//! Hydra-compatible expression parser for experiment parameter sweeps.
//!
//! Supports the sweep forms DVC exposes through `--set-param`:
//! - `choice(a,b,c)` — explicit choice sweep
//! - `range(stop)`, `range(start, stop)`, or `range(start, stop, step)`
//! - `val1,val2,val3` — comma-separated choice shorthand
//!
//! Returns a `Vec<String>` of expanded values suitable for use as
//! parameter override values in experiment queue entries.

use crate::{Result, WorkflowError as CrabError};

/// Parse a `--set-param` value expression and expand it into
/// individual string values.
///
/// Recognizes:
/// - `choice(val1, val2)` — Hydra choice sweep.
/// - `range(...)` — Hydra stop-exclusive numeric range sweep.
/// - `val1,val2,val3` — splits on commas and trims whitespace.
/// - A plain value with no commas and no `range(...)` wrapper returns
///   a single-element vec.
pub fn expand_param_value(expr: &str) -> Result<Vec<String>> {
    let trimmed = expr.trim();

    if let Some(inner) = function_inner(trimmed, "range") {
        return parse_range(trimmed, inner);
    }
    if let Some(inner) = function_inner(trimmed, "choice") {
        return parse_choice(trimmed, inner);
    }

    if trimmed.contains(',') {
        let values = split_sweep_args(trimmed)?;
        if values.len() > 1 {
            return Ok(values);
        }
    }

    Ok(vec![trimmed.to_owned()])
}

/// Return whether an expression uses Hydra sweep syntax.
pub fn is_sweep_expression(expr: &str) -> Result<bool> {
    let trimmed = expr.trim();
    if function_inner(trimmed, "range").is_some() || function_inner(trimmed, "choice").is_some() {
        return Ok(true);
    }
    if !trimmed.contains(',') {
        return Ok(false);
    }
    Ok(split_sweep_args(trimmed)?.len() > 1)
}

fn function_inner<'a>(expr: &'a str, name: &str) -> Option<&'a str> {
    let prefix = format!("{name}(");
    expr.strip_prefix(&prefix)?.strip_suffix(')')
}

fn parse_choice(expr: &str, inner: &str) -> Result<Vec<String>> {
    let values = split_sweep_args(inner)?;
    if values.is_empty() {
        return Err(CrabError::Configuration {
            key: format!("choice() requires at least one value: {expr}"),
            origin: "experiment".into(),
        });
    }
    Ok(values)
}

/// Parse Hydra `range(...)` into expanded values.
///
/// Hydra ranges are stop-exclusive: positive steps include values
/// `< stop`, and negative steps include values `> stop`.
fn parse_range(expr: &str, inner: &str) -> Result<Vec<String>> {
    let raw_args = split_sweep_args(inner)?;
    let range = parse_range_args(expr, &raw_args)?;

    if range.step == 0.0 {
        return Err(CrabError::Configuration {
            key: format!("range() step must be non-zero: {expr}"),
            origin: "experiment".into(),
        });
    }

    if (range.stop - range.start).signum() != range.step.signum()
        && (range.stop - range.start).abs() > f64::EPSILON
    {
        return Err(CrabError::Configuration {
            key: format!(
                "range() step direction does not reach stop: start={}, stop={}, step={}",
                range.start, range.stop, range.step
            ),
            origin: "experiment".into(),
        });
    }

    let mut values = Vec::new();
    let mut i = 0u64;
    loop {
        let value = range.start + (i as f64) * range.step;
        let epsilon = f64::EPSILON * range.stop.abs().max(range.start.abs()).max(1.0);

        if range.step > 0.0 && value >= range.stop - epsilon {
            break;
        }
        if range.step < 0.0 && value <= range.stop + epsilon {
            break;
        }

        values.push(format_range_value(value, range.force_float));
        i += 1;

        if values.len() > 10_000 {
            return Err(CrabError::Configuration {
                key: format!("range() would produce more than 10000 values: {expr}"),
                origin: "experiment".into(),
            });
        }
    }

    if values.is_empty() {
        return Err(CrabError::Configuration {
            key: format!("range() produced no values: {expr}"),
            origin: "experiment".into(),
        });
    }

    Ok(values)
}

#[derive(Debug)]
struct RangeArgs {
    start: f64,
    stop: f64,
    step: f64,
    force_float: bool,
}

fn parse_range_args(expr: &str, raw_args: &[String]) -> Result<RangeArgs> {
    let mut positional = Vec::new();
    let mut named_start = None;
    let mut named_stop = None;
    let mut named_step = None;
    let mut force_float = false;

    for raw_arg in raw_args {
        if let Some((name, value)) = raw_arg.split_once('=') {
            let value = value.trim();
            let parsed = parse_range_number(expr, value)?;
            force_float |= looks_floaty(value);
            match name.trim() {
                "start" if named_start.is_none() => named_start = Some(parsed),
                "stop" if named_stop.is_none() => named_stop = Some(parsed),
                "step" if named_step.is_none() => named_step = Some(parsed),
                "start" | "stop" | "step" => {
                    return Err(CrabError::Configuration {
                        key: format!("range() argument repeated: {name} in {expr}"),
                        origin: "experiment".into(),
                    });
                }
                other => {
                    return Err(CrabError::Configuration {
                        key: format!("range() unsupported named argument '{other}': {expr}"),
                        origin: "experiment".into(),
                    });
                }
            }
        } else {
            force_float |= looks_floaty(raw_arg);
            positional.push(parse_range_number(expr, raw_arg)?);
        }
    }

    let mut step = named_step.unwrap_or(1.0);
    let (start, stop) = match positional.as_slice() {
        [] => {
            let stop = named_stop.ok_or_else(|| CrabError::Configuration {
                key: format!("range() requires a stop value: {expr}"),
                origin: "experiment".into(),
            })?;
            (named_start.unwrap_or(0.0), stop)
        }
        [stop] if named_start.is_none() && named_stop.is_none() => (0.0, *stop),
        [start, stop] if named_start.is_none() && named_stop.is_none() => (*start, *stop),
        [start, stop, positional_step]
            if named_start.is_none() && named_stop.is_none() && named_step.is_none() =>
        {
            step = *positional_step;
            (*start, *stop)
        }
        _ => {
            return Err(CrabError::Configuration {
                key: format!(
                    "range() expects range(stop), range(start, stop), or range(start, stop, step): {expr}"
                ),
                origin: "experiment".into(),
            });
        }
    };

    Ok(RangeArgs {
        start,
        stop,
        step,
        force_float,
    })
}

fn parse_range_number(expr: &str, raw: &str) -> Result<f64> {
    raw.parse().map_err(|_| CrabError::Configuration {
        key: format!("range() argument is not a number: '{raw}' in {expr}"),
        origin: "experiment".into(),
    })
}

fn looks_floaty(raw: &str) -> bool {
    raw.contains('.') || raw.contains('e') || raw.contains('E')
}

fn format_range_value(value: f64, force_float: bool) -> String {
    if force_float && value == value.trunc() && value.abs() < (i64::MAX as f64) {
        return format!("{value:.1}");
    }
    format_numeric(value)
}

fn split_sweep_args(expr: &str) -> Result<Vec<String>> {
    let mut args = Vec::new();
    let mut start = 0usize;
    let mut depth = 0i32;
    let mut quote = None;
    let mut escaped = false;

    for (idx, ch) in expr.char_indices() {
        if let Some(quote_ch) = quote {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == quote_ch {
                quote = None;
            }
            continue;
        }

        match ch {
            '\'' | '"' => quote = Some(ch),
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => {
                depth -= 1;
                if depth < 0 {
                    return Err(CrabError::Configuration {
                        key: format!("unbalanced sweep expression: {expr}"),
                        origin: "experiment".into(),
                    });
                }
            }
            ',' if depth == 0 => {
                push_sweep_arg(expr, &mut args, &expr[start..idx])?;
                start = idx + ch.len_utf8();
            }
            _ => {}
        }
    }

    if quote.is_some() || depth != 0 {
        return Err(CrabError::Configuration {
            key: format!("unbalanced sweep expression: {expr}"),
            origin: "experiment".into(),
        });
    }

    push_sweep_arg(expr, &mut args, &expr[start..])?;
    Ok(args)
}

fn push_sweep_arg(expr: &str, args: &mut Vec<String>, raw: &str) -> Result<()> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(CrabError::Configuration {
            key: format!("empty value in sweep expression: {expr}"),
            origin: "experiment".into(),
        });
    }
    args.push(trimmed.to_owned());
    Ok(())
}

/// Format a numeric value: integers without decimal point, floats
/// with minimal precision to avoid trailing zeros.
fn format_numeric(v: f64) -> String {
    // Check if the value is an exact integer.
    if v == v.trunc() && v.abs() < (i64::MAX as f64) {
        format!("{}", v as i64)
    } else {
        // Use enough precision to round-trip but strip trailing zeros.
        let s = format!("{:.10}", v);
        let s = s.trim_end_matches('0');
        let s = s.trim_end_matches('.');
        s.to_owned()
    }
}

/// Compute the Cartesian product of multiple parameter value lists.
///
/// Given a list of `(key, values)` pairs, returns all combinations
/// as a vec of `BTreeMap<key, value>`. The order is lexicographic
/// over the input order (first key varies slowest).
pub fn cartesian_product(
    params: &[(String, Vec<String>)],
) -> Vec<std::collections::BTreeMap<String, String>> {
    use std::collections::BTreeMap;

    if params.is_empty() {
        return vec![BTreeMap::new()];
    }

    let mut result: Vec<BTreeMap<String, String>> = vec![BTreeMap::new()];

    for (key, values) in params {
        let mut next = Vec::with_capacity(result.len() * values.len());
        for existing in &result {
            for val in values {
                let mut combo = existing.clone();
                combo.insert(key.clone(), val.clone());
                next.push(combo);
            }
        }
        result = next;
    }

    result
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "test assertions")]
mod tests {
    use super::*;

    #[test]
    fn range_basic_float() {
        let values = expand_param_value("range(0.001, 0.01, 0.003)").unwrap();
        assert_eq!(values, vec!["0.001", "0.004", "0.007"]);
    }

    #[test]
    fn range_integer() {
        let values = expand_param_value("range(1, 5, 2)").unwrap();
        assert_eq!(values, vec!["1", "3"]);
    }

    #[test]
    fn range_stop_only() {
        let values = expand_param_value("range(5)").unwrap();
        assert_eq!(values, vec!["0", "1", "2", "3", "4"]);
    }

    #[test]
    fn range_start_stop_defaults_step() {
        let values = expand_param_value("range(0, 5)").unwrap();
        assert_eq!(values, vec!["0", "1", "2", "3", "4"]);
    }

    #[test]
    fn range_named_step() {
        let values = expand_param_value("range(-5, step=-1)").unwrap();
        assert_eq!(values, vec!["0", "-1", "-2", "-3", "-4"]);
    }

    #[test]
    fn range_zero_step_rejected() {
        let err = expand_param_value("range(1, 5, 0)").unwrap_err();
        assert!(matches!(err, CrabError::Configuration { .. }));
    }

    #[test]
    fn range_wrong_direction_rejected() {
        let err = expand_param_value("range(5, 1, 1)").unwrap_err();
        assert!(matches!(err, CrabError::Configuration { .. }));
    }

    #[test]
    fn range_descending() {
        let values = expand_param_value("range(10, 1, -3)").unwrap();
        assert_eq!(values, vec!["10", "7", "4"]);
    }

    #[test]
    fn range_float_step_formats_integral_values_as_float() {
        let values = expand_param_value("range(0, 10, 3.3)").unwrap();
        assert_eq!(values, vec!["0.0", "3.3", "6.6", "9.9"]);
    }

    #[test]
    fn choice_function_values() {
        let values = expand_param_value("choice(resnet, efficientnet)").unwrap();
        assert_eq!(values, vec!["resnet", "efficientnet"]);
    }

    #[test]
    fn comma_separated_values() {
        let values = expand_param_value("resnet,vgg,efficientnet").unwrap();
        assert_eq!(values, vec!["resnet", "vgg", "efficientnet"]);
    }

    #[test]
    fn detects_sweep_expressions_for_queue_only_paths() {
        assert!(is_sweep_expression("choice(resnet, efficientnet)").unwrap());
        assert!(is_sweep_expression("range(1, 4)").unwrap());
        assert!(is_sweep_expression("resnet,vgg").unwrap());
        assert!(!is_sweep_expression("'resnet,vgg'").unwrap());
        assert!(!is_sweep_expression("[1, 2]").unwrap());
        assert!(!is_sweep_expression("resnet").unwrap());
    }

    #[test]
    fn comma_separated_with_spaces() {
        let values = expand_param_value("a, b , c").unwrap();
        assert_eq!(values, vec!["a", "b", "c"]);
    }

    #[test]
    fn nested_comma_value_is_not_split_as_sweep() {
        let values = expand_param_value("[1, 2]").unwrap();
        assert_eq!(values, vec!["[1, 2]"]);
    }

    #[test]
    fn plain_scalar() {
        let values = expand_param_value("0.01").unwrap();
        assert_eq!(values, vec!["0.01"]);
    }

    #[test]
    fn plain_string() {
        let values = expand_param_value("resnet").unwrap();
        assert_eq!(values, vec!["resnet"]);
    }

    #[test]
    fn cartesian_product_two_params() {
        let params = vec![
            ("lr".to_owned(), vec!["0.01".to_owned(), "0.1".to_owned()]),
            (
                "arch".to_owned(),
                vec!["resnet".to_owned(), "vgg".to_owned()],
            ),
        ];
        let combos = cartesian_product(&params);
        assert_eq!(combos.len(), 4);
        assert_eq!(combos[0]["lr"], "0.01");
        assert_eq!(combos[0]["arch"], "resnet");
        assert_eq!(combos[1]["lr"], "0.01");
        assert_eq!(combos[1]["arch"], "vgg");
        assert_eq!(combos[2]["lr"], "0.1");
        assert_eq!(combos[2]["arch"], "resnet");
        assert_eq!(combos[3]["lr"], "0.1");
        assert_eq!(combos[3]["arch"], "vgg");
    }

    #[test]
    fn cartesian_product_empty() {
        let params: Vec<(String, Vec<String>)> = vec![];
        let combos = cartesian_product(&params);
        assert_eq!(combos.len(), 1);
        assert!(combos[0].is_empty());
    }

    #[test]
    fn cartesian_product_single_param() {
        let params = vec![(
            "x".to_owned(),
            vec!["1".to_owned(), "2".to_owned(), "3".to_owned()],
        )];
        let combos = cartesian_product(&params);
        assert_eq!(combos.len(), 3);
    }
}
