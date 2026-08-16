//! `foreach` stage expansion.
//!
//! Takes a stage definition containing `foreach:` + `do:` fields and
//! expands it into N concrete stages — one per item in the iteration
//! source. Each expanded stage has its `${item}`, `${key}`, and
//! `${index}` references resolved against the current iteration item.
//!
//! Three iteration forms are supported:
//! - List of scalars: suffix is the value (`stage@value`)
//! - List of dicts: suffix is the index (`stage@0`, `stage@1`)
//! - Dict: suffix is the key (`stage@uk`, `stage@us`)

use serde_yaml::Value;

use crate::StageName;
use crate::template::{TemplateContext, substitute, substitute_cmd};
use crate::{Result, WorkflowError};

/// Expand a `foreach` stage into concrete `(StageName, Value)` pairs.
///
/// `base_name` is the stage key from the YAML (e.g. `"preprocess"`).
/// `foreach_value` is the resolved `foreach:` field (list or dict).
/// `do_template` is the `do:` block — a YAML mapping representing
/// the stage template with `${item}` / `${key}` / `${index}` placeholders.
/// `global_ctx` is the workflow-level template context for resolving
/// any non-item variables that appear in the template.
///
/// Returns a vec of `(expanded_name, resolved_stage_value)` pairs
/// ready to be parsed as regular stages.
pub fn expand_foreach(
    base_name: &str,
    foreach_value: &Value,
    do_template: &Value,
    global_ctx: &TemplateContext,
) -> Result<Vec<(StageName, Value)>> {
    match foreach_value {
        Value::Sequence(items) => {
            if items.is_empty() {
                return Err(WorkflowError::ForeachEmpty {
                    stage: base_name.to_owned(),
                });
            }
            expand_list(base_name, items, do_template, global_ctx)
        }
        Value::Mapping(map) => {
            if map.is_empty() {
                return Err(WorkflowError::ForeachEmpty {
                    stage: base_name.to_owned(),
                });
            }
            expand_dict(base_name, map, do_template, global_ctx)
        }
        _ => Err(WorkflowError::TemplateInvalid {
            key: format!("stage '{base_name}' foreach"),
            origin: "foreach value must be a list or a dict".to_owned(),
        }),
    }
}

/// Expand a list-form `foreach`.
///
/// - List of scalars → suffix is the scalar value.
/// - List of dicts → suffix is the index.
fn expand_list(
    base_name: &str,
    items: &[Value],
    do_template: &Value,
    global_ctx: &TemplateContext,
) -> Result<Vec<(StageName, Value)>> {
    let mut results = Vec::with_capacity(items.len());

    for (index, item) in items.iter().enumerate() {
        let suffix = compute_list_suffix(item, index);
        let expanded_name = make_expanded_name(base_name, &suffix)?;

        // Build item-local context overlay.
        let item_ctx = build_item_context(item, &suffix, index);

        // Substitute the do: template with the item context + global context.
        let resolved = substitute_template(do_template, &item_ctx, global_ctx, base_name)?;

        results.push((expanded_name, resolved));
    }

    Ok(results)
}

/// Expand a dict-form `foreach`.
///
/// Keys become the suffix; values become `${item}`.
fn expand_dict(
    base_name: &str,
    map: &serde_yaml::Mapping,
    do_template: &Value,
    global_ctx: &TemplateContext,
) -> Result<Vec<(StageName, Value)>> {
    let mut results = Vec::with_capacity(map.len());

    for (index, (key, value)) in map.iter().enumerate() {
        let key_str =
            value_to_scalar_string(key).ok_or_else(|| WorkflowError::TemplateInvalid {
                key: format!("stage '{base_name}' foreach"),
                origin: "dict keys in foreach must be scalar strings".to_owned(),
            })?;

        let expanded_name = make_expanded_name(base_name, &key_str)?;

        // Build item-local context: item = value, key = key_str, index = index.
        let item_ctx = build_item_context(value, &key_str, index);

        let resolved = substitute_template(do_template, &item_ctx, global_ctx, base_name)?;

        results.push((expanded_name, resolved));
    }

    Ok(results)
}

/// Determine the suffix for a list item.
///
/// - Scalar values → the value itself (sanitized for stage names).
/// - Dict/Sequence values → the index.
fn compute_list_suffix(item: &Value, index: usize) -> String {
    match item {
        Value::String(s) => sanitize_suffix(s),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        // Complex items (dicts, lists) use the index as suffix.
        _ => index.to_string(),
    }
}

/// Sanitize a string for use as a stage name suffix.
///
/// Replaces characters not valid in stage names with underscores.
/// The suffix portion after `@` follows the same character rules as
/// the base name (ASCII alphanumeric, underscore, dash).
fn sanitize_suffix(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    for ch in s.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
            result.push(ch);
        } else {
            result.push('_');
        }
    }
    // Ensure the suffix is not empty after sanitization.
    if result.is_empty() {
        result.push('_');
    }
    result
}

/// Build an expanded stage name: `base@suffix`.
///
/// The `@` separator is not valid in the base stage-name grammar,
/// which prevents collisions with user-defined names.
fn make_expanded_name(base_name: &str, suffix: &str) -> Result<StageName> {
    let full = format!("{base_name}@{suffix}");
    StageName::parse(&full)
}

/// Build an item-local template context for foreach substitution.
///
/// The returned context contains:
/// - `item` → the current item value (scalar or nested mapping)
/// - `key` → the suffix/key string
/// - `index` → the numeric index
fn build_item_context(item: &Value, key: &str, index: usize) -> TemplateContext {
    let mut vars = serde_yaml::Mapping::new();

    // For scalar items, set `item` directly.
    // For complex items (mappings), set `item` as the mapping so
    // `${item.field}` works via dotted-path resolution.
    vars.insert(Value::String("item".to_owned()), item.clone());
    vars.insert(
        Value::String("key".to_owned()),
        Value::String(key.to_owned()),
    );
    vars.insert(
        Value::String("index".to_owned()),
        Value::String(index.to_string()),
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
/// context. This allows `${item}` and `${key}` to resolve from the
/// foreach iteration while `${codedir}` resolves from vars/params.
fn substitute_template(
    template: &Value,
    item_ctx: &TemplateContext,
    global_ctx: &TemplateContext,
    stage_name: &str,
) -> Result<Value> {
    match template {
        Value::String(s) => {
            // Try item-local context first, fall back to global.
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
        // Non-string scalars pass through unchanged.
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
///
/// Uses a combined context that overlays item vars on top of global
/// vars so that `${item}` resolves locally while `${codedir}` still
/// resolves from the workflow-level context.
fn substitute_with_fallback(
    input: &str,
    item_ctx: &TemplateContext,
    global_ctx: &TemplateContext,
    stage_name: &str,
) -> Result<String> {
    // Build a merged context: item vars overlay global vars.
    let merged = merge_contexts(item_ctx, global_ctx);
    substitute(input, &merged).map_err(|e| match e {
        WorkflowError::TemplateUndefined { key, .. } => WorkflowError::TemplateUndefined {
            key,
            field: "foreach do".to_owned(),
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
            field: "foreach do".to_owned(),
            stage: stage_name.to_owned(),
        },
        other => other,
    })
}

/// Merge two contexts: item-local vars take precedence over global.
///
/// This is a shallow merge — the item context's vars overlay the
/// global context's vars. Params from the global context are preserved.
fn merge_contexts(item_ctx: &TemplateContext, global_ctx: &TemplateContext) -> TemplateContext {
    // The item context has vars (item, key, index) but no params.
    // The global context has both vars and params.
    // We want: vars = global_vars + item_vars (item wins on conflict),
    //          params = global_params.
    //
    // Since TemplateContext resolves params before vars, and we want
    // item vars to win over everything for `item`/`key`/`index`, we
    // put item vars into the params slot of a new context (higher
    // precedence) and global vars+params into the vars slot.
    //
    // Actually, the resolution order is: params > vars. So we put
    // item-local values as params (highest precedence) and keep
    // global context's vars and params merged as the vars layer.
    //
    // Simpler approach: build a single TemplateContext where:
    // - params = item vars (item, key, index) — highest precedence
    // - vars = merged global vars + global params
    //
    // This way ${item} resolves from params (item context) and
    // ${codedir} resolves from vars (global context).

    // Extract the item vars as the "params" (high precedence).
    // The item_ctx was built with vars = {item, key, index}, params = empty.
    // We'll use a trick: create a new context where the item's vars
    // become params (so they take precedence).
    let item_vars = item_ctx.vars_value();
    let global_vars = global_ctx.vars_value();
    let global_params = global_ctx.params_value();

    // Merge global vars and global params into one mapping for the
    // "vars" layer. Global params override global vars (preserving
    // existing precedence).
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
    fn expand_list_of_strings() {
        let foreach_val: Value = serde_yaml::from_str("[raw_a, raw_b, raw_c]").unwrap();
        let do_template: Value = serde_yaml::from_str(
            r#"
            cmd: "python clean.py ${item}"
            deps:
              - "${item}.csv"
            outs:
              - "${item}_clean.csv"
            "#,
        )
        .unwrap();

        let ctx = TemplateContext::empty();
        let results = expand_foreach("preprocess", &foreach_val, &do_template, &ctx).unwrap();

        assert_eq!(results.len(), 3);
        assert_eq!(results[0].0.as_str(), "preprocess@raw_a");
        assert_eq!(results[1].0.as_str(), "preprocess@raw_b");
        assert_eq!(results[2].0.as_str(), "preprocess@raw_c");

        // Verify substitution in the first expanded stage.
        let stage0 = &results[0].1;
        let cmd = stage0.get("cmd").unwrap().as_str().unwrap();
        assert_eq!(cmd, "python clean.py raw_a");
    }

    #[test]
    fn expand_dict_form() {
        let foreach_val: Value = serde_yaml::from_str(
            r#"
            uk:
              region: eu-west-1
              bucket: data-uk
            us:
              region: us-east-1
              bucket: data-us
            "#,
        )
        .unwrap();
        let do_template: Value = serde_yaml::from_str(
            r#"
            cmd: "python sync.py --region ${item.region} --bucket ${item.bucket}"
            "#,
        )
        .unwrap();

        let ctx = TemplateContext::empty();
        let results = expand_foreach("build", &foreach_val, &do_template, &ctx).unwrap();

        assert_eq!(results.len(), 2);
        // Dict iteration order in serde_yaml is insertion order.
        assert_eq!(results[0].0.as_str(), "build@uk");
        assert_eq!(results[1].0.as_str(), "build@us");

        let cmd0 = results[0].1.get("cmd").unwrap().as_str().unwrap();
        assert!(cmd0.contains("eu-west-1"));
        assert!(cmd0.contains("data-uk"));
    }

    #[test]
    fn expand_list_of_dicts_uses_index_suffix() {
        let foreach_val: Value = serde_yaml::from_str(
            r#"
            - name: alpha
              lr: 0.01
            - name: beta
              lr: 0.001
            "#,
        )
        .unwrap();
        let do_template: Value = serde_yaml::from_str(
            r#"
            cmd: "python train.py --name ${item.name} --lr ${item.lr}"
            "#,
        )
        .unwrap();

        let ctx = TemplateContext::empty();
        let results = expand_foreach("train", &foreach_val, &do_template, &ctx).unwrap();

        assert_eq!(results.len(), 2);
        assert_eq!(results[0].0.as_str(), "train@0");
        assert_eq!(results[1].0.as_str(), "train@1");

        let cmd0 = results[0].1.get("cmd").unwrap().as_str().unwrap();
        assert_eq!(cmd0, "python train.py --name alpha --lr 0.01");
    }

    #[test]
    fn empty_list_returns_error() {
        let foreach_val: Value = serde_yaml::from_str("[]").unwrap();
        let do_template: Value = serde_yaml::from_str("cmd: echo hi").unwrap();
        let ctx = TemplateContext::empty();

        let err = expand_foreach("stage", &foreach_val, &do_template, &ctx).unwrap_err();
        assert!(matches!(err, WorkflowError::ForeachEmpty { .. }));
    }

    #[test]
    fn empty_dict_returns_error() {
        let foreach_val: Value = serde_yaml::from_str("{}").unwrap();
        let do_template: Value = serde_yaml::from_str("cmd: echo hi").unwrap();
        let ctx = TemplateContext::empty();

        let err = expand_foreach("stage", &foreach_val, &do_template, &ctx).unwrap_err();
        assert!(matches!(err, WorkflowError::ForeachEmpty { .. }));
    }

    #[test]
    fn global_context_vars_available_in_template() {
        let foreach_val: Value = serde_yaml::from_str("[a, b]").unwrap();
        let do_template: Value = serde_yaml::from_str(
            r#"
            cmd: "python ${codedir}/clean.py ${item}"
            "#,
        )
        .unwrap();

        let vars: Value = serde_yaml::from_str("codedir: src").unwrap();
        let ctx = TemplateContext::new(vars, Value::Mapping(serde_yaml::Mapping::new()), false);

        let results = expand_foreach("clean", &foreach_val, &do_template, &ctx).unwrap();
        let cmd0 = results[0].1.get("cmd").unwrap().as_str().unwrap();
        assert_eq!(cmd0, "python src/clean.py a");
    }

    #[test]
    fn key_and_index_available_in_template() {
        let foreach_val: Value = serde_yaml::from_str("[alpha, beta]").unwrap();
        let do_template: Value = serde_yaml::from_str(
            r#"
            cmd: "echo ${key} ${index} ${item}"
            "#,
        )
        .unwrap();

        let ctx = TemplateContext::empty();
        let results = expand_foreach("test", &foreach_val, &do_template, &ctx).unwrap();

        let cmd0 = results[0].1.get("cmd").unwrap().as_str().unwrap();
        assert_eq!(cmd0, "echo alpha 0 alpha");

        let cmd1 = results[1].1.get("cmd").unwrap().as_str().unwrap();
        assert_eq!(cmd1, "echo beta 1 beta");
    }

    #[test]
    fn sanitize_suffix_replaces_invalid_chars() {
        assert_eq!(sanitize_suffix("hello world"), "hello_world");
        assert_eq!(sanitize_suffix("a/b/c"), "a_b_c");
        assert_eq!(sanitize_suffix("valid-name_123"), "valid-name_123");
        assert_eq!(sanitize_suffix(""), "_");
    }

    #[test]
    fn make_expanded_name_validates_base() {
        let err = make_expanded_name("123invalid", "suffix").unwrap_err();
        assert!(matches!(err, WorkflowError::StageNameInvalid { .. }));
    }

    #[test]
    fn make_expanded_name_rejects_empty_suffix() {
        let err = make_expanded_name("valid", "").unwrap_err();
        assert!(matches!(err, WorkflowError::StageNameInvalid { .. }));
    }
}
