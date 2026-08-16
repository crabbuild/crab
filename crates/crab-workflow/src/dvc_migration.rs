//! DVC → crab YAML migration.
//!
//! Parses a subset of DVC's `dvc.yaml` schema and converts it to
//! `crab.yaml` format. Handles `foreach`, `matrix`, `vars`,
//! templating passthrough, and maps `always_changed` →
//! `nondeterministic`. Emits warnings for unsupported features.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde_yaml::Value;

use crate::error::{Result, WorkflowError};

/// A warning emitted during DVC → crab conversion for features that
/// cannot be automatically migrated.
#[derive(Debug, Clone)]
pub struct MigrationWarning {
    pub stage: String,
    pub message: String,
}

/// Summary of a DVC → crab migration.
#[derive(Debug)]
pub struct MigrationReport {
    pub stages_converted: usize,
    pub warnings: Vec<MigrationWarning>,
    pub output_path: Option<PathBuf>,
}

/// Parse a DVC YAML file and convert it to crab YAML content.
///
/// Returns the generated YAML string and a migration report.
pub fn convert_dvc_to_crab(dvc_content: &str) -> Result<(String, MigrationReport)> {
    let dvc: Value = serde_yaml::from_str(dvc_content)
        .map_err(|source| WorkflowError::DvcYamlParse { source })?;

    let mut warnings: Vec<MigrationWarning> = Vec::new();
    let mut crab_doc: BTreeMap<String, Value> = BTreeMap::new();
    let mut stages_converted: usize = 0;

    // Convert top-level metadata/file lists that share syntax with crab.
    for key in ["vars", "artifacts", "params", "metrics"] {
        if let Some(value) = dvc.get(key) {
            crab_doc.insert(key.into(), value.clone());
        }
    }

    let mut crab_stages: BTreeMap<String, Value> = BTreeMap::new();

    // Convert `stages:` when present. DVC also allows dvc.yaml files
    // that only declare artifacts, metrics, params, or plots.
    if let Some(stages_value) = dvc.get("stages") {
        let stages =
            stages_value
                .as_mapping()
                .ok_or_else(|| WorkflowError::DvcMigrationInvalid {
                    key: "dvc.yaml `stages:` must be a mapping".into(),
                    origin: "dvc.yaml".into(),
                })?;

        for (name_val, stage_val) in stages {
            let name = name_val.as_str().unwrap_or("<unknown>").to_owned();

            let Some(stage_map) = stage_val.as_mapping() else {
                warnings.push(MigrationWarning {
                    stage: name.clone(),
                    message: "stage is not a mapping; skipped".into(),
                });
                continue;
            };

            let converted = convert_stage(&name, stage_map, &mut warnings);
            crab_stages.insert(name, converted);
            stages_converted += 1;
        }
    }

    if !crab_stages.is_empty() {
        crab_doc.insert(
            "stages".into(),
            serde_yaml::to_value(&crab_stages)
                .map_err(|source| WorkflowError::DvcYamlSerialize { source })?,
        );
    }

    // Convert top-level `plots:` if present (simplified passthrough).
    if let Some(plots) = dvc.get("plots") {
        crab_doc.insert("plots".into(), plots.clone());
    }

    if crab_doc.is_empty() {
        return Err(WorkflowError::DvcMigrationInvalid {
            key: "dvc.yaml did not contain supported workflow metadata".into(),
            origin: "dvc.yaml".into(),
        });
    }

    let yaml_out = serde_yaml::to_string(&crab_doc)
        .map_err(|source| WorkflowError::DvcYamlSerialize { source })?;

    let report = MigrationReport {
        stages_converted,
        warnings,
        output_path: None,
    };

    Ok((yaml_out, report))
}

/// Convert a single DVC stage mapping to crab format.
fn convert_stage(
    name: &str,
    stage: &serde_yaml::Mapping,
    warnings: &mut Vec<MigrationWarning>,
) -> Value {
    let mut out = serde_yaml::Mapping::new();
    let mut live_paths = Vec::new();

    for (key_val, val) in stage {
        let Some(key) = key_val.as_str() else {
            continue;
        };

        match key {
            // Direct copy fields.
            "deps" | "params" | "metrics" | "wdir" | "frozen" | "desc" | "meta" | "vars"
            | "plots" | "foreach" | "matrix" => {
                out.insert(key_val.clone(), val.clone());
            }

            // `cmd:` — strings and DVC shell lists pass through.
            "cmd" => {
                out.insert(key_val.clone(), val.clone());
            }

            // `outs:` — normalize DVC path-key settings.
            "outs" => {
                let converted_outs = convert_outs(name, val, warnings);
                out.insert(key_val.clone(), converted_outs);
            }

            // `always_changed:` → `nondeterministic:`.
            "always_changed" => {
                out.insert(Value::String("nondeterministic".into()), val.clone());
            }

            // `do:` — the stage template inside a foreach block.
            "do" => {
                // Recursively convert the `do:` block.
                if let Some(do_map) = val.as_mapping() {
                    let converted_do = convert_stage(name, do_map, warnings);
                    out.insert(key_val.clone(), converted_do);
                } else {
                    out.insert(key_val.clone(), val.clone());
                }
            }

            // Unsupported DVC-specific features — emit warnings.
            "live" => {
                live_paths.extend(convert_live_paths(name, val, warnings));
            }

            // Unknown fields — pass through with a warning.
            other => {
                warnings.push(MigrationWarning {
                    stage: name.into(),
                    message: format!("unknown field `{other}` passed through as-is"),
                });
                out.insert(key_val.clone(), val.clone());
            }
        }
    }

    append_live_outputs(name, &mut out, &live_paths, warnings);

    Value::Mapping(out)
}

/// Convert DVC `outs:` list, emitting warnings for unsupported fields.
fn convert_outs(stage_name: &str, val: &Value, warnings: &mut Vec<MigrationWarning>) -> Value {
    let Some(seq) = val.as_sequence() else {
        return val.clone();
    };

    let mut converted: Vec<Value> = Vec::with_capacity(seq.len());

    for item in seq {
        match item {
            // Structured outputs can be DVC's path-key form
            // (`out.bin: {cache: false}`) or an explicit `path:`
            // mapping. Normalize both to Crab's strict shape.
            Value::Mapping(m) => {
                if let Some(path) = mapping_string(m, "path") {
                    let filtered = filter_dvc_out_settings(stage_name, m, warnings);
                    converted.push(structured_out(&path, filtered));
                    continue;
                }

                for (k, v) in m {
                    let Some(path) = k.as_str() else {
                        converted.push(item.clone());
                        continue;
                    };
                    if let Some(settings) = v.as_mapping() {
                        let filtered = filter_dvc_out_settings(stage_name, settings, warnings);
                        if filtered.is_empty() {
                            converted.push(Value::String(path.to_owned()));
                        } else {
                            converted.push(structured_out(path, filtered));
                        }
                    } else {
                        converted.push(Value::String(path.to_owned()));
                    }
                }
            }
            // Simple strings and unsupported item shapes pass through.
            _ => {
                converted.push(item.clone());
            }
        }
    }

    Value::Sequence(converted)
}

fn mapping_string(map: &serde_yaml::Mapping, key: &str) -> Option<String> {
    map.get(Value::String(key.to_owned()))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn convert_live_paths(
    stage_name: &str,
    val: &Value,
    warnings: &mut Vec<MigrationWarning>,
) -> Vec<String> {
    let mut paths = Vec::new();
    collect_live_paths(stage_name, val, warnings, &mut paths);
    paths
}

fn collect_live_paths(
    stage_name: &str,
    val: &Value,
    warnings: &mut Vec<MigrationWarning>,
    paths: &mut Vec<String>,
) {
    match val {
        Value::String(path) => {
            push_live_path(paths, path);
        }
        Value::Bool(true) => {
            push_live_path(paths, "dvclive");
        }
        Value::Bool(false) | Value::Null => {}
        Value::Sequence(seq) => {
            for item in seq {
                collect_live_paths(stage_name, item, warnings, paths);
            }
        }
        Value::Mapping(map) => {
            if let Some(path) = mapping_string(map, "path").or_else(|| mapping_string(map, "dir")) {
                push_live_path(paths, &path);
                return;
            }

            let mut saw_path_key = false;
            let mut only_options = true;
            for (key, _) in map {
                let Some(key) = key.as_str() else {
                    only_options = false;
                    warnings.push(MigrationWarning {
                        stage: stage_name.into(),
                        message: "non-string `live:` key removed".into(),
                    });
                    continue;
                };

                if is_live_option_key(key) {
                    continue;
                }

                saw_path_key = true;
                only_options = false;
                push_live_path(paths, key);
            }

            if !saw_path_key && only_options {
                push_live_path(paths, "dvclive");
            }
        }
        _ => {
            warnings.push(MigrationWarning {
                stage: stage_name.into(),
                message: "`live:` shape is not supported; removed".into(),
            });
        }
    }
}

fn push_live_path(paths: &mut Vec<String>, path: &str) {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return;
    }
    if !paths
        .iter()
        .any(|existing| same_path_text(existing, trimmed))
    {
        paths.push(trimmed.to_owned());
    }
}

fn is_live_option_key(key: &str) -> bool {
    matches!(
        key,
        "summary"
            | "html"
            | "report"
            | "resume"
            | "cache_images"
            | "save_dvc_exp"
            | "monitor_system"
    )
}

fn append_live_outputs(
    stage_name: &str,
    out: &mut serde_yaml::Mapping,
    live_paths: &[String],
    warnings: &mut Vec<MigrationWarning>,
) {
    for live_path in live_paths {
        append_sequence_value(
            stage_name,
            out,
            "outs",
            live_path,
            live_directory_out(live_path),
            warnings,
        );

        let metrics_path = live_child_path(live_path, "metrics.json");
        append_sequence_value(
            stage_name,
            out,
            "metrics",
            &metrics_path,
            Value::String(metrics_path.clone()),
            warnings,
        );

        let plots_path = live_child_path(live_path, "plots");
        append_sequence_value(
            stage_name,
            out,
            "plots",
            &plots_path,
            Value::String(plots_path.clone()),
            warnings,
        );
    }
}

fn append_sequence_value(
    stage_name: &str,
    out: &mut serde_yaml::Mapping,
    field: &str,
    path: &str,
    value: Value,
    warnings: &mut Vec<MigrationWarning>,
) {
    let field_key = Value::String(field.to_owned());
    match out.get_mut(&field_key) {
        Some(Value::Sequence(seq)) => {
            if !sequence_contains_path(seq, path) {
                seq.push(value);
            }
        }
        Some(_) => {
            warnings.push(MigrationWarning {
                stage: stage_name.into(),
                message: format!("cannot append migrated `live:` path to non-list `{field}:`"),
            });
        }
        None => {
            out.insert(field_key, Value::Sequence(vec![value]));
        }
    }
}

fn sequence_contains_path(seq: &[Value], path: &str) -> bool {
    seq.iter()
        .filter_map(value_path)
        .any(|existing| same_path_text(&existing, path))
}

fn value_path(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Mapping(map) => mapping_string(map, "path").or_else(|| {
            if map.len() == 1 {
                map.keys().next().and_then(Value::as_str).map(str::to_owned)
            } else {
                None
            }
        }),
        _ => None,
    }
}

fn same_path_text(a: &str, b: &str) -> bool {
    a.trim_end_matches('/') == b.trim_end_matches('/')
}

fn live_child_path(live_path: &str, child: &str) -> String {
    let base = live_path.trim_end_matches('/');
    if base.is_empty() {
        child.to_owned()
    } else {
        format!("{base}/{child}")
    }
}

fn live_directory_out(path: &str) -> Value {
    let mut settings = serde_yaml::Mapping::new();
    settings.insert(
        Value::String("kind".to_owned()),
        Value::String("directory".to_owned()),
    );
    structured_out(path, settings)
}

fn filter_dvc_out_settings(
    stage_name: &str,
    settings: &serde_yaml::Mapping,
    warnings: &mut Vec<MigrationWarning>,
) -> serde_yaml::Mapping {
    let mut filtered = serde_yaml::Mapping::new();
    let mut checkpoint_persist = false;
    for (sk, sv) in settings {
        let setting_key = sk.as_str().unwrap_or("");
        match setting_key {
            "path" | "desc" => {}
            "cache" | "push" | "persist" | "kind" | "max_bytes" | "remote" => {
                filtered.insert(sk.clone(), sv.clone());
            }
            "checkpoint" => {
                checkpoint_persist = sv.as_bool().unwrap_or(false);
            }
            _ => {
                warnings.push(MigrationWarning {
                    stage: stage_name.into(),
                    message: format!("unknown output field `{setting_key}` removed"),
                });
            }
        }
    }
    if checkpoint_persist {
        filtered.insert(Value::String("persist".to_owned()), Value::Bool(true));
    }
    filtered
}

fn structured_out(path: &str, settings: serde_yaml::Mapping) -> Value {
    let mut out = serde_yaml::Mapping::new();
    out.insert(
        Value::String("path".to_owned()),
        Value::String(path.to_owned()),
    );
    for (key, value) in settings {
        out.insert(key, value);
    }
    Value::Mapping(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_doc(yaml: &str) -> Value {
        serde_yaml::from_str(yaml).unwrap()
    }

    fn key<'a>(map: &'a serde_yaml::Mapping, name: &str) -> &'a Value {
        map.get(Value::String(name.to_owned())).unwrap()
    }

    fn stage<'a>(doc: &'a Value, name: &str) -> &'a serde_yaml::Mapping {
        let root = doc.as_mapping().unwrap();
        let stages = key(root, "stages").as_mapping().unwrap();
        key(stages, name).as_mapping().unwrap()
    }

    fn stage_sequence<'a>(doc: &'a Value, stage_name: &str, field: &str) -> &'a Vec<Value> {
        key(stage(doc, stage_name), field).as_sequence().unwrap()
    }

    fn value_path(value: &Value) -> &str {
        match value {
            Value::String(path) => path,
            Value::Mapping(map) => key(map, "path").as_str().unwrap(),
            _ => unreachable!("expected path-like value"),
        }
    }

    #[test]
    fn converts_simple_pipeline() {
        let dvc = r#"
stages:
  clean:
    cmd: python clean.py
    deps:
      - raw_data.csv
    outs:
      - clean_data.csv
  train:
    cmd: python train.py
    deps:
      - clean_data.csv
      - train.py
    outs:
      - model.pkl
    params:
      - train.lr
      - train.epochs
    metrics:
      - metrics.json
"#;
        let (yaml, report) = convert_dvc_to_crab(dvc).unwrap();
        assert_eq!(report.stages_converted, 2);
        assert!(report.warnings.is_empty());
        assert!(yaml.contains("clean"));
        assert!(yaml.contains("train"));
        assert!(yaml.contains("python clean.py"));
        assert!(yaml.contains("model.pkl"));
    }

    #[test]
    fn preserves_cmd_list_as_separate_shell_commands() {
        let dvc = r#"
stages:
  build:
    cmd:
      - mkdir -p output
      - python build.py
      - python validate.py
    deps:
      - build.py
    outs:
      - output/
"#;
        let (yaml, report) = convert_dvc_to_crab(dvc).unwrap();
        assert_eq!(report.stages_converted, 1);
        assert!(yaml.contains("- mkdir -p output"));
        assert!(yaml.contains("- python build.py"));
        assert!(yaml.contains("- python validate.py"));
    }

    #[test]
    fn maps_always_changed_to_nondeterministic() {
        let dvc = r#"
stages:
  fetch_data:
    cmd: python fetch.py
    always_changed: true
    outs:
      - data.csv
"#;
        let (yaml, report) = convert_dvc_to_crab(dvc).unwrap();
        assert_eq!(report.stages_converted, 1);
        assert!(yaml.contains("nondeterministic"));
        assert!(!yaml.contains("always_changed"));
    }

    #[test]
    fn converts_push_false_to_crab_output_push_policy() {
        let dvc = r#"
stages:
  train:
    cmd: python train.py
    outs:
      - model.pkl:
          cache: true
          push: false
"#;
        let (yaml, report) = convert_dvc_to_crab(dvc).unwrap();
        assert!(report.warnings.is_empty());

        let doc = parse_doc(&yaml);
        let outs = stage_sequence(&doc, "train", "outs");
        let out = outs[0].as_mapping().unwrap();
        assert_eq!(key(out, "path").as_str(), Some("model.pkl"));
        assert_eq!(key(out, "cache").as_bool(), Some(true));
        assert_eq!(key(out, "push").as_bool(), Some(false));
    }

    #[test]
    fn converts_dvc_path_key_out_settings_to_crab_structured_out() {
        let dvc = r#"
stages:
  train:
    cmd: python train.py
    outs:
      - model.pkl:
          cache: false
          persist: true
          desc: local intermediate
"#;
        let (yaml, report) = convert_dvc_to_crab(dvc).unwrap();
        assert!(report.warnings.is_empty());

        let doc = parse_doc(&yaml);
        let outs = stage_sequence(&doc, "train", "outs");
        let out = outs[0].as_mapping().unwrap();
        assert_eq!(key(out, "path").as_str(), Some("model.pkl"));
        assert_eq!(key(out, "cache").as_bool(), Some(false));
        assert_eq!(key(out, "persist").as_bool(), Some(true));
    }

    #[test]
    fn converts_dvc_path_field_out_settings_to_crab_structured_out() {
        let dvc = r#"
stages:
  train:
    cmd: python train.py
    outs:
      - path: model.pkl
        cache: false
        persist: true
"#;
        let (yaml, report) = convert_dvc_to_crab(dvc).unwrap();
        assert!(report.warnings.is_empty());

        let doc = parse_doc(&yaml);
        let outs = stage_sequence(&doc, "train", "outs");
        let out = outs[0].as_mapping().unwrap();
        assert_eq!(key(out, "path").as_str(), Some("model.pkl"));
        assert_eq!(key(out, "cache").as_bool(), Some(false));
        assert_eq!(key(out, "persist").as_bool(), Some(true));
    }

    #[test]
    fn preserves_per_output_remote() {
        let dvc = r#"
stages:
  train:
    cmd: python train.py
    outs:
      - model.pkl:
          remote: myremote
"#;
        let (yaml, report) = convert_dvc_to_crab(dvc).unwrap();
        assert!(report.warnings.is_empty());

        let doc = parse_doc(&yaml);
        let outs = stage_sequence(&doc, "train", "outs");
        let out = outs[0].as_mapping().unwrap();
        assert_eq!(key(out, "remote").as_str(), Some("myremote"));
    }

    #[test]
    fn passes_through_foreach() {
        let dvc = r#"
stages:
  preprocess:
    foreach:
      - raw_a
      - raw_b
      - raw_c
    do:
      cmd: "python clean.py ${item}"
      deps:
        - ${item}.csv
      outs:
        - ${item}_clean.csv
"#;
        let (yaml, report) = convert_dvc_to_crab(dvc).unwrap();
        assert_eq!(report.stages_converted, 1);
        assert!(yaml.contains("foreach"));
        assert!(yaml.contains("${item}"));
    }

    #[test]
    fn passes_through_matrix() {
        let dvc = r#"
stages:
  train:
    matrix:
      model: [resnet, vgg]
      dataset: [imagenet, cifar10]
    cmd: "python train.py --model ${item.model} --data ${item.dataset}"
    deps:
      - train.py
    outs:
      - models/${item.model}-${item.dataset}.pkl
"#;
        let (yaml, report) = convert_dvc_to_crab(dvc).unwrap();
        assert_eq!(report.stages_converted, 1);
        assert!(yaml.contains("matrix"));
        assert!(yaml.contains("${item.model}"));
    }

    #[test]
    fn migrated_stage_level_dvc_plot_configs_parse() {
        let dvc = r#"
stages:
  train:
    cmd: python train.py
    plots:
      - train_val_test:
          y:
            train.csv: [train_acc, val_acc]
            test.csv: test_acc
          x: epoch
"#;
        let (yaml, report) = convert_dvc_to_crab(dvc).unwrap();
        assert_eq!(report.stages_converted, 1);

        let doc = parse_doc(&yaml);
        let plots = stage_sequence(&doc, "train", "plots");
        assert_eq!(plots.len(), 1);
        assert!(yaml.contains("train_val_test"));
        assert!(yaml.contains("train.csv"));
        assert!(yaml.contains("test.csv"));
    }

    #[test]
    fn passes_through_vars() {
        let dvc = r#"
vars:
  - codedir: src
  - config/extra.yaml

stages:
  train:
    cmd: "python ${codedir}/train.py"
    deps:
      - ${codedir}/train.py
    outs:
      - model.pkl
"#;
        let (yaml, report) = convert_dvc_to_crab(dvc).unwrap();
        assert_eq!(report.stages_converted, 1);
        assert!(yaml.contains("vars"));
        assert!(yaml.contains("codedir"));
        assert!(yaml.contains("${codedir}"));
    }

    #[test]
    fn passes_through_top_level_params_metrics_and_file_scoped_stage_params() {
        let dvc = r#"
params:
  - params.yaml
  - custom.yaml
metrics:
  - reports/summary.json

stages:
  train:
    cmd: python train.py
    params:
      - model.lr
      - custom.yaml:
          - model.dropout
      - all.json:
    metrics:
      - metrics/train.json
    outs:
      - model.pkl
"#;
        let (yaml, report) = convert_dvc_to_crab(dvc).unwrap();
        assert_eq!(report.stages_converted, 1);

        let doc = parse_doc(&yaml);
        let root = doc.as_mapping().unwrap();
        let params = key(root, "params").as_sequence().unwrap();
        let metrics = key(root, "metrics").as_sequence().unwrap();
        assert_eq!(params[0].as_str(), Some("params.yaml"));
        assert_eq!(params[1].as_str(), Some("custom.yaml"));
        assert_eq!(metrics[0].as_str(), Some("reports/summary.json"));

        let stage_params = stage_sequence(&doc, "train", "params");
        assert_eq!(stage_params[0].as_str(), Some("model.lr"));
        assert!(
            stage_params[1]
                .as_mapping()
                .unwrap()
                .contains_key(Value::String("custom.yaml".to_owned()))
        );
        assert!(
            stage_params[2]
                .as_mapping()
                .unwrap()
                .contains_key(Value::String("all.json".to_owned()))
        );
    }

    #[test]
    fn migrated_stage_metric_settings_parse_as_metric_output() {
        let dvc = r#"
stages:
  train:
    cmd: python train.py
    metrics:
      - metrics/train.json:
          cache: false
          persist: true
          push: false
"#;
        let (yaml, report) = convert_dvc_to_crab(dvc).unwrap();
        assert_eq!(report.stages_converted, 1);

        let doc = parse_doc(&yaml);
        let metrics = stage_sequence(&doc, "train", "metrics");
        let metric = metrics[0].as_mapping().unwrap();
        assert!(metric.contains_key(Value::String("metrics/train.json".to_owned())));
        let settings = metric
            .get(Value::String("metrics/train.json".to_owned()))
            .unwrap()
            .as_mapping()
            .unwrap();
        assert_eq!(key(settings, "cache").as_bool(), Some(false));
        assert_eq!(key(settings, "persist").as_bool(), Some(true));
        assert_eq!(key(settings, "push").as_bool(), Some(false));
    }

    #[test]
    fn passes_through_top_level_artifacts_metadata() {
        let dvc = r#"
artifacts:
  cv-classification:
    path: models/resnet.pt
    type: model
    desc: CV classification model
    labels:
      - resnet50
      - classification
    meta:
      framework: pytorch

stages:
  train:
    cmd: python train.py
    outs:
      - models/resnet.pt
"#;
        let (yaml, report) = convert_dvc_to_crab(dvc).unwrap();
        assert_eq!(report.stages_converted, 1);
        assert!(yaml.contains("artifacts"));
        assert!(yaml.contains("cv-classification"));
        assert!(yaml.contains("models/resnet.pt"));
        let doc = parse_doc(&yaml);
        assert!(
            doc.as_mapping()
                .unwrap()
                .contains_key(Value::String("artifacts".to_owned()))
        );
    }

    #[test]
    fn migrates_artifacts_only_dvc_yaml() {
        let dvc = r#"
artifacts:
  cv-classification:
    path: models/resnet.pt
    type: model
"#;
        let (yaml, report) = convert_dvc_to_crab(dvc).unwrap();
        assert_eq!(report.stages_converted, 0);
        assert!(yaml.contains("artifacts"));
        assert!(!yaml.contains("stages"));
        let doc = parse_doc(&yaml);
        assert!(
            !doc.as_mapping()
                .unwrap()
                .contains_key(Value::String("stages".to_owned()))
        );
    }

    #[test]
    fn passes_through_wdir_and_frozen() {
        let dvc = r#"
stages:
  train:
    cmd: python train.py
    wdir: training/
    frozen: true
    deps:
      - data.csv
    outs:
      - model.pkl
"#;
        let (yaml, report) = convert_dvc_to_crab(dvc).unwrap();
        assert_eq!(report.stages_converted, 1);
        assert!(yaml.contains("wdir"));
        assert!(yaml.contains("frozen"));
    }

    #[test]
    fn passes_through_desc_and_meta() {
        let dvc = r#"
stages:
  train:
    cmd: python train.py
    desc: "Train the model"
    meta:
      team: ml-platform
      cost_center: research
    deps:
      - train.py
    outs:
      - model.pkl
"#;
        let (yaml, report) = convert_dvc_to_crab(dvc).unwrap();
        assert_eq!(report.stages_converted, 1);
        assert!(yaml.contains("desc"));
        assert!(yaml.contains("meta"));
        assert!(yaml.contains("Train the model"));
    }

    #[test]
    fn migrates_live_mapping_to_directory_out_and_metric() {
        let dvc = r#"
stages:
  train:
    cmd: python train.py
    live:
      dvclive:
        summary: true
        html: true
    outs:
      - model.pkl
"#;
        let (yaml, report) = convert_dvc_to_crab(dvc).unwrap();
        assert!(report.warnings.is_empty());

        let doc = parse_doc(&yaml);
        let metrics = stage_sequence(&doc, "train", "metrics");
        let plots = stage_sequence(&doc, "train", "plots");
        assert_eq!(metrics[0].as_str(), Some("dvclive/metrics.json"));
        assert_eq!(plots[0].as_str(), Some("dvclive/plots"));

        let outs = stage_sequence(&doc, "train", "outs");
        let live_out = outs
            .iter()
            .filter_map(Value::as_mapping)
            .find(|out| key(out, "path").as_str() == Some("dvclive"))
            .unwrap();
        assert_eq!(key(live_out, "kind").as_str(), Some("directory"));
    }

    #[test]
    fn migrates_live_string_and_options_only_default_dir() {
        let dvc = r#"
stages:
  train:
    cmd: python train.py
    live:
      - runs/live
      - summary: true
        html: true
"#;
        let (yaml, report) = convert_dvc_to_crab(dvc).unwrap();
        assert!(report.warnings.is_empty());

        let doc = parse_doc(&yaml);
        let metric_paths: Vec<_> = stage_sequence(&doc, "train", "metrics")
            .iter()
            .map(Value::as_str)
            .collect();
        assert_eq!(
            metric_paths,
            vec![Some("runs/live/metrics.json"), Some("dvclive/metrics.json")]
        );
        let plot_paths: Vec<_> = stage_sequence(&doc, "train", "plots")
            .iter()
            .map(Value::as_str)
            .collect();
        assert_eq!(
            plot_paths,
            vec![Some("runs/live/plots"), Some("dvclive/plots")]
        );
        let out_paths: Vec<_> = stage_sequence(&doc, "train", "outs")
            .iter()
            .map(value_path)
            .collect();
        assert!(out_paths.contains(&"runs/live"));
        assert!(out_paths.contains(&"dvclive"));
    }

    #[test]
    fn live_migration_deduplicates_existing_outs_and_metrics() {
        let dvc = r#"
stages:
  train:
    cmd: python train.py
    live: dvclive
    outs:
      - dvclive:
          kind: directory
    metrics:
      - dvclive/metrics.json
"#;
        let (yaml, report) = convert_dvc_to_crab(dvc).unwrap();
        assert!(report.warnings.is_empty());

        let doc = parse_doc(&yaml);
        assert_eq!(
            stage_sequence(&doc, "train", "outs")
                .iter()
                .filter(|out| value_path(out) == "dvclive")
                .count(),
            1
        );
        assert_eq!(
            stage_sequence(&doc, "train", "metrics")
                .iter()
                .filter(|path| path.as_str() == Some("dvclive/metrics.json"))
                .count(),
            1
        );
        assert_eq!(
            stage_sequence(&doc, "train", "plots")
                .iter()
                .filter(|path| path.as_str() == Some("dvclive/plots"))
                .count(),
            1
        );
    }

    #[test]
    fn converts_checkpoint_output_to_persistent_out() {
        let dvc = r#"
stages:
  train:
    cmd: python train.py
    outs:
      - model.pt:
          checkpoint: true
          push: false
"#;
        let (yaml, report) = convert_dvc_to_crab(dvc).unwrap();
        assert!(report.warnings.is_empty());

        let doc = parse_doc(&yaml);
        let outs = stage_sequence(&doc, "train", "outs");
        let out = outs[0].as_mapping().unwrap();
        assert_eq!(key(out, "path").as_str(), Some("model.pt"));
        assert_eq!(key(out, "persist").as_bool(), Some(true));
        assert_eq!(key(out, "push").as_bool(), Some(false));
    }

    #[test]
    fn error_on_unsupported_metadata_only_yaml() {
        let dvc = r#"
foo:
  bar: baz
"#;
        let result = convert_dvc_to_crab(dvc);
        assert!(result.is_err());
    }
}
