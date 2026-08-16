//! `matrix` stage expansion (Cartesian product).
//!
//! Takes a stage definition containing `matrix:` (a dict of variable
//! names to value lists) and expands it into one concrete stage per
//! Cartesian product combination. Each expanded stage has its
//! `${item.var}` and `${key}` references resolved against the current
//! combination.
//!
//! Example:
//! ```yaml
//! stages:
//!   train:
//!     matrix:
//!       model: [resnet, vgg]
//!       dataset: [imagenet, cifar10]
//!     cmd: "python train.py --model ${item.model} --data ${item.dataset}"
//! ```
//!
//! Produces: `train@resnet-imagenet`, `train@resnet-cifar10`,
//! `train@vgg-imagenet`, `train@vgg-cifar10`.

use serde_yaml::Value;

use crate::StageName;
use crate::template::{TemplateContext, substitute, substitute_cmd};
use crate::{Result, WorkflowError};

#[derive(Debug, Clone, PartialEq)]
struct MatrixItem {
    value: Value,
    suffix: String,
}

/// Expand a `matrix` stage into concrete `(StageName, Value)` pairs.
///
/// `base_name` is the stage key from the YAML (e.g. `"train"`).
/// `matrix_value` is the `matrix:` field — a mapping of variable names
/// to lists of values.
/// `stage_template` is the rest of the stage definition (cmd, deps,
/// outs, etc.) with `${item.var}` / `${key}` placeholders.
/// `global_ctx` is the workflow-level template context for resolving
/// any non-item variables that appear in the template.
///
/// Returns a vec of `(expanded_name, resolved_stage_value)` pairs
/// ready to be parsed as regular stages.
pub fn expand_matrix(
    base_name: &str,
    matrix_value: &Value,
    stage_template: &Value,
    global_ctx: &TemplateContext,
) -> Result<Vec<(StageName, Value)>> {
    let mapping = match matrix_value {
        Value::Mapping(m) => m,
        _ => {
            return Err(WorkflowError::TemplateInvalid {
                key: format!("stage '{base_name}' matrix"),
                origin: "matrix value must be a mapping of variable names to value lists"
                    .to_owned(),
            });
        }
    };

    if mapping.is_empty() {
        return Err(WorkflowError::TemplateInvalid {
            key: format!("stage '{base_name}' matrix"),
            origin: "matrix must contain at least one variable".to_owned(),
        });
    }

    // Extract variable names and their value lists in insertion order.
    let mut var_names: Vec<String> = Vec::with_capacity(mapping.len());
    let mut var_values: Vec<Vec<MatrixItem>> = Vec::with_capacity(mapping.len());

    for (key, val) in mapping {
        let var_name =
            value_to_scalar_string(key).ok_or_else(|| WorkflowError::TemplateInvalid {
                key: format!("stage '{base_name}' matrix"),
                origin: "matrix variable names must be scalar strings".to_owned(),
            })?;

        let values = match val {
            Value::Sequence(seq) => {
                if seq.is_empty() {
                    return Err(WorkflowError::MatrixEmpty {
                        stage: base_name.to_owned(),
                        variable: var_name,
                    });
                }
                seq.iter()
                    .enumerate()
                    .map(|(index, value)| MatrixItem {
                        value: value.clone(),
                        suffix: matrix_item_suffix(&var_name, index, value),
                    })
                    .collect::<Vec<_>>()
            }
            _ => {
                return Err(WorkflowError::TemplateInvalid {
                    key: format!("stage '{base_name}' matrix.{var_name}"),
                    origin: "matrix variable values must be a list".to_owned(),
                });
            }
        };

        var_names.push(var_name);
        var_values.push(values);
    }

    // Compute the Cartesian product of all variable value lists.
    let combinations = cartesian_product(&var_values);

    let mut results = Vec::with_capacity(combinations.len());

    for combo in &combinations {
        // Build the hyphen-joined key: e.g. "resnet-imagenet"
        let key_str = combo
            .iter()
            .map(|item| item.suffix.as_str())
            .collect::<Vec<_>>()
            .join("-");

        // Build the expanded stage name: base@val1-val2
        let expanded_name = make_expanded_name(base_name, &key_str)?;

        // Build item-local context with item.var_name for each variable.
        let item_ctx = build_matrix_context(&var_names, combo, &key_str);

        // Substitute the stage template.
        let resolved = substitute_template(stage_template, &item_ctx, global_ctx, base_name)?;

        results.push((expanded_name, resolved));
    }

    Ok(results)
}

fn matrix_item_suffix(var_name: &str, index: usize, value: &Value) -> String {
    value_to_scalar_string(value).unwrap_or_else(|| format!("{var_name}{index}"))
}

/// Compute the Cartesian product of a list of value lists.
///
/// Given `[[a, b], [1, 2, 3]]`, produces:
/// `[[a, 1], [a, 2], [a, 3], [b, 1], [b, 2], [b, 3]]`
fn cartesian_product<T: Clone>(lists: &[Vec<T>]) -> Vec<Vec<T>> {
    if lists.is_empty() {
        return vec![vec![]];
    }

    let mut result: Vec<Vec<T>> = vec![vec![]];

    for list in lists {
        let mut new_result = Vec::with_capacity(result.len() * list.len());
        for existing in &result {
            for item in list {
                let mut combo = existing.clone();
                combo.push(item.clone());
                new_result.push(combo);
            }
        }
        result = new_result;
    }

    result
}

/// Build an expanded stage name: `base@suffix`.
fn make_expanded_name(base_name: &str, suffix: &str) -> Result<StageName> {
    let sanitized = sanitize_suffix(suffix);
    let full = format!("{base_name}@{sanitized}");
    StageName::parse(&full)
}

/// Sanitize a string for use as a stage name suffix.
///
/// Replaces characters not valid in stage names with underscores.
fn sanitize_suffix(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    for ch in s.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
            result.push(ch);
        } else {
            result.push('_');
        }
    }
    if result.is_empty() {
        result.push('_');
    }
    result
}

/// Build a template context for a matrix combination.
///
/// The context contains:
/// - `item.var_name` for each variable in the matrix
/// - `key` → the hyphen-joined combination string
fn build_matrix_context(var_names: &[String], values: &[MatrixItem], key: &str) -> TemplateContext {
    let mut item_map = serde_yaml::Mapping::new();
    for (name, item) in var_names.iter().zip(values.iter()) {
        item_map.insert(Value::String(name.clone()), item.value.clone());
    }

    let mut vars = serde_yaml::Mapping::new();
    vars.insert(Value::String("item".to_owned()), Value::Mapping(item_map));
    vars.insert(
        Value::String("key".to_owned()),
        Value::String(key.to_owned()),
    );

    TemplateContext::new(
        Value::Mapping(vars),
        Value::Mapping(serde_yaml::Mapping::new()),
        false,
    )
}

/// Recursively substitute `${...}` expressions in a YAML value tree.
///
/// Tries the item-local context first, then falls back to the global
/// context.
fn substitute_template(
    template: &Value,
    item_ctx: &TemplateContext,
    global_ctx: &TemplateContext,
    stage_name: &str,
) -> Result<Value> {
    match template {
        Value::String(s) => {
            let resolved = substitute_with_fallback(s, item_ctx, global_ctx, stage_name)?;
            Ok(Value::String(resolved))
        }
        Value::Mapping(map) => {
            let mut resolved_map = serde_yaml::Mapping::new();
            for (k, v) in map {
                let resolved_key = substitute_template(k, item_ctx, global_ctx, stage_name)?;
                let command_field =
                    matches!(&resolved_key, Value::String(s) if s == "cmd" || s == "on_cache_hit");
                let resolved_val = if command_field {
                    substitute_command_template(v, item_ctx, global_ctx, stage_name)?
                } else {
                    substitute_template(v, item_ctx, global_ctx, stage_name)?
                };
                resolved_map.insert(resolved_key, resolved_val);
            }
            Ok(Value::Mapping(resolved_map))
        }
        Value::Sequence(seq) => {
            let resolved: Result<Vec<Value>> = seq
                .iter()
                .map(|v| substitute_template(v, item_ctx, global_ctx, stage_name))
                .collect();
            Ok(Value::Sequence(resolved?))
        }
        other => Ok(other.clone()),
    }
}

fn substitute_command_template(
    template: &Value,
    item_ctx: &TemplateContext,
    global_ctx: &TemplateContext,
    stage_name: &str,
) -> Result<Value> {
    match template {
        Value::String(s) => {
            let resolved = substitute_command_with_fallback(s, item_ctx, global_ctx, stage_name)?;
            Ok(Value::String(resolved))
        }
        other => substitute_template(other, item_ctx, global_ctx, stage_name),
    }
}

/// Substitute a string, trying the item context first, then global.
fn substitute_with_fallback(
    input: &str,
    item_ctx: &TemplateContext,
    global_ctx: &TemplateContext,
    stage_name: &str,
) -> Result<String> {
    let merged = merge_contexts(item_ctx, global_ctx);
    substitute(input, &merged).map_err(|e| match e {
        WorkflowError::TemplateUndefined { key, .. } => WorkflowError::TemplateUndefined {
            key,
            field: "matrix stage".to_owned(),
            stage: stage_name.to_owned(),
        },
        other => other,
    })
}

fn substitute_command_with_fallback(
    input: &str,
    item_ctx: &TemplateContext,
    global_ctx: &TemplateContext,
    stage_name: &str,
) -> Result<String> {
    let merged = merge_contexts(item_ctx, global_ctx);
    substitute_cmd(input, &merged).map_err(|e| match e {
        WorkflowError::TemplateUndefined { key, .. } => WorkflowError::TemplateUndefined {
            key,
            field: "matrix stage".to_owned(),
            stage: stage_name.to_owned(),
        },
        other => other,
    })
}

/// Merge two contexts: item-local vars take precedence over global.
fn merge_contexts(item_ctx: &TemplateContext, global_ctx: &TemplateContext) -> TemplateContext {
    let item_vars = item_ctx.vars_value();
    let global_vars = global_ctx.vars_value();
    let global_params = global_ctx.params_value();

    let mut merged_vars = serde_yaml::Mapping::new();
    if let Value::Mapping(gv) = global_vars {
        for (k, v) in gv {
            merged_vars.insert(k.clone(), v.clone());
        }
    }
    if let Value::Mapping(gp) = global_params {
        for (k, v) in gp {
            merged_vars.insert(k.clone(), v.clone());
        }
    }

    TemplateContext::new(
        Value::Mapping(merged_vars),
        item_vars.clone(),
        global_ctx.env_enabled(),
    )
}

/// Convert a YAML value to a scalar string if possible.
fn value_to_scalar_string(val: &Value) -> Option<String> {
    match val {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        Value::Null => Some("null".to_owned()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_2x3_matrix() {
        let matrix_val: Value = serde_yaml::from_str(
            r#"
            model: [resnet, vgg]
            dataset: [imagenet, cifar10, coco]
            "#,
        )
        .unwrap();
        let stage_template: Value = serde_yaml::from_str(
            r#"
            cmd: "python train.py --model ${item.model} --data ${item.dataset}"
            deps:
              - "data/${item.dataset}/"
            outs:
              - "models/${item.model}-${item.dataset}.pkl"
            "#,
        )
        .unwrap();

        let ctx = TemplateContext::empty();
        let results = expand_matrix("train", &matrix_val, &stage_template, &ctx).unwrap();

        assert_eq!(results.len(), 6);

        // Verify expanded names follow the @val1-val2 pattern.
        let names: Vec<&str> = results.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"train@resnet-imagenet"));
        assert!(names.contains(&"train@resnet-cifar10"));
        assert!(names.contains(&"train@resnet-coco"));
        assert!(names.contains(&"train@vgg-imagenet"));
        assert!(names.contains(&"train@vgg-cifar10"));
        assert!(names.contains(&"train@vgg-coco"));

        // Verify substitution in one of the expanded stages.
        let resnet_imagenet = results
            .iter()
            .find(|(n, _)| n.as_str() == "train@resnet-imagenet")
            .unwrap();
        let cmd = resnet_imagenet.1.get("cmd").unwrap().as_str().unwrap();
        assert_eq!(cmd, "python train.py --model resnet --data imagenet");

        let deps = resnet_imagenet
            .1
            .get("deps")
            .unwrap()
            .as_sequence()
            .unwrap();
        assert_eq!(deps[0].as_str().unwrap(), "data/imagenet/");

        let outs = resnet_imagenet
            .1
            .get("outs")
            .unwrap()
            .as_sequence()
            .unwrap();
        assert_eq!(outs[0].as_str().unwrap(), "models/resnet-imagenet.pkl");
    }

    #[test]
    fn key_resolves_to_hyphen_joined_values() {
        let matrix_val: Value = serde_yaml::from_str(
            r#"
            model: [resnet]
            dataset: [imagenet]
            "#,
        )
        .unwrap();
        let stage_template: Value = serde_yaml::from_str(
            r#"
            cmd: "echo ${key}"
            "#,
        )
        .unwrap();

        let ctx = TemplateContext::empty();
        let results = expand_matrix("train", &matrix_val, &stage_template, &ctx).unwrap();

        assert_eq!(results.len(), 1);
        let cmd = results[0].1.get("cmd").unwrap().as_str().unwrap();
        assert_eq!(cmd, "echo resnet-imagenet");
    }

    #[test]
    fn complex_values_use_index_suffixes_and_preserve_item_tree() {
        let matrix_val: Value = serde_yaml::from_str(
            r#"
            labels:
              - [label1, label2, label3]
            config:
              - n_estimators: 150
                max_depth: 20
            "#,
        )
        .unwrap();
        let stage_template: Value = serde_yaml::from_str(
            r#"
            cmd: "python train.py --trees ${item.config.n_estimators} --label ${item.labels[1]}"
            outs:
              - "${key}.pkl"
            "#,
        )
        .unwrap();

        let ctx = TemplateContext::empty();
        let results = expand_matrix("train", &matrix_val, &stage_template, &ctx).unwrap();

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0.as_str(), "train@labels0-config0");
        let cmd = results[0].1.get("cmd").unwrap().as_str().unwrap();
        assert_eq!(cmd, "python train.py --trees 150 --label label2");
        let outs = results[0].1.get("outs").unwrap().as_sequence().unwrap();
        assert_eq!(outs[0].as_str().unwrap(), "labels0-config0.pkl");
    }

    #[test]
    fn empty_variable_list_returns_error() {
        let matrix_val: Value = serde_yaml::from_str(
            r#"
            model: [resnet, vgg]
            dataset: []
            "#,
        )
        .unwrap();
        let stage_template: Value = serde_yaml::from_str("cmd: echo hi").unwrap();
        let ctx = TemplateContext::empty();

        let err = expand_matrix("train", &matrix_val, &stage_template, &ctx).unwrap_err();
        match err {
            WorkflowError::MatrixEmpty { stage, variable } => {
                assert_eq!(stage, "train");
                assert_eq!(variable, "dataset");
            }
            other => panic!("expected WorkflowMatrixEmpty, got {other}"),
        }
    }

    #[test]
    fn non_mapping_matrix_returns_error() {
        let matrix_val: Value = serde_yaml::from_str("[a, b, c]").unwrap();
        let stage_template: Value = serde_yaml::from_str("cmd: echo hi").unwrap();
        let ctx = TemplateContext::empty();

        let err = expand_matrix("train", &matrix_val, &stage_template, &ctx).unwrap_err();
        assert!(matches!(err, WorkflowError::TemplateInvalid { .. }));
    }

    #[test]
    fn single_variable_matrix() {
        let matrix_val: Value = serde_yaml::from_str(
            r#"
            lr: ["0.001", "0.01", "0.1"]
            "#,
        )
        .unwrap();
        let stage_template: Value = serde_yaml::from_str(
            r#"
            cmd: "python train.py --lr ${item.lr}"
            "#,
        )
        .unwrap();

        let ctx = TemplateContext::empty();
        let results = expand_matrix("sweep", &matrix_val, &stage_template, &ctx).unwrap();

        assert_eq!(results.len(), 3);
        assert_eq!(results[0].0.as_str(), "sweep@0_001");
        assert_eq!(results[1].0.as_str(), "sweep@0_01");
        assert_eq!(results[2].0.as_str(), "sweep@0_1");

        let cmd0 = results[0].1.get("cmd").unwrap().as_str().unwrap();
        assert_eq!(cmd0, "python train.py --lr 0.001");
    }

    #[test]
    fn global_context_available_in_matrix_template() {
        let matrix_val: Value = serde_yaml::from_str(
            r#"
            model: [resnet]
            "#,
        )
        .unwrap();
        let stage_template: Value = serde_yaml::from_str(
            r#"
            cmd: "python ${codedir}/train.py --model ${item.model}"
            "#,
        )
        .unwrap();

        let vars: Value = serde_yaml::from_str("codedir: src").unwrap();
        let ctx = TemplateContext::new(vars, Value::Mapping(serde_yaml::Mapping::new()), false);

        let results = expand_matrix("train", &matrix_val, &stage_template, &ctx).unwrap();
        let cmd = results[0].1.get("cmd").unwrap().as_str().unwrap();
        assert_eq!(cmd, "python src/train.py --model resnet");
    }

    #[test]
    fn cartesian_product_correctness() {
        let lists = vec![
            vec!["a".to_owned(), "b".to_owned()],
            vec!["1".to_owned(), "2".to_owned(), "3".to_owned()],
        ];
        let product = cartesian_product(&lists);
        assert_eq!(product.len(), 6);
        assert_eq!(product[0], vec!["a", "1"]);
        assert_eq!(product[1], vec!["a", "2"]);
        assert_eq!(product[2], vec!["a", "3"]);
        assert_eq!(product[3], vec!["b", "1"]);
        assert_eq!(product[4], vec!["b", "2"]);
        assert_eq!(product[5], vec!["b", "3"]);
    }

    #[test]
    fn cartesian_product_single_list() {
        let lists = vec![vec!["x".to_owned(), "y".to_owned()]];
        let product = cartesian_product(&lists);
        assert_eq!(product.len(), 2);
        assert_eq!(product[0], vec!["x"]);
        assert_eq!(product[1], vec!["y"]);
    }

    #[test]
    fn cartesian_product_empty_input() {
        let lists: Vec<Vec<String>> = vec![];
        let product = cartesian_product(&lists);
        assert_eq!(product.len(), 1);
        assert!(product[0].is_empty());
    }

    #[test]
    fn numeric_matrix_values() {
        let matrix_val: Value = serde_yaml::from_str(
            r#"
            batch_size: [16, 32, 64]
            epochs: [10, 50]
            "#,
        )
        .unwrap();
        let stage_template: Value = serde_yaml::from_str(
            r#"
            cmd: "python train.py --batch ${item.batch_size} --epochs ${item.epochs}"
            "#,
        )
        .unwrap();

        let ctx = TemplateContext::empty();
        let results = expand_matrix("train", &matrix_val, &stage_template, &ctx).unwrap();

        assert_eq!(results.len(), 6);
        assert_eq!(results[0].0.as_str(), "train@16-10");

        let cmd = results[0].1.get("cmd").unwrap().as_str().unwrap();
        assert_eq!(cmd, "python train.py --batch 16 --epochs 10");
    }

    #[test]
    fn sanitize_suffix_handles_special_chars() {
        assert_eq!(sanitize_suffix("hello-world"), "hello-world");
        assert_eq!(sanitize_suffix("a.b.c"), "a_b_c");
        assert_eq!(sanitize_suffix(""), "_");
        assert_eq!(sanitize_suffix("valid_name-123"), "valid_name-123");
    }
}
