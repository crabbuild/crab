//! `TemplateContext` — merged variable resolution for `${...}` expressions.
//!
//! Loads values from three sources in increasing precedence:
//! 1. Inline `vars:` block (list of maps or file references)
//! 2. Params files (YAML, JSON, TOML, Python literal assignments)
//! 3. Environment variables (gated by `env_enabled`)
//!
//! Params take precedence over vars on key conflicts. Environment
//! variables are accessed via the `env.` prefix and are only
//! available when `env_enabled` is true.

use std::path::Path;

use serde_yaml::Value;

use crate::params::{self, Scalar};
use crate::{Result, WorkflowError};

/// Merged template context for `${...}` resolution.
///
/// Vars are structural (not tracked in stage hash). Params are
/// tunable inputs that participate in the stage hash.
#[derive(Debug, Clone)]
pub struct TemplateContext {
    /// Vars (not tracked in stage hash).
    vars: Value,
    /// Params (tracked in stage hash).
    params: Value,
    /// Whether `${env.VAR}` access is enabled.
    env_enabled: bool,
}

impl TemplateContext {
    /// Build a context from pre-merged vars and params values.
    ///
    /// Both `vars` and `params` should be `Value::Mapping` at the
    /// root level. A `Value::Null` is treated as an empty mapping.
    pub fn new(vars: Value, params: Value, env_enabled: bool) -> Self {
        Self {
            vars,
            params,
            env_enabled,
        }
    }

    /// Build a context with no vars, no params, and env disabled.
    pub fn empty() -> Self {
        Self {
            vars: Value::Mapping(serde_yaml::Mapping::new()),
            params: Value::Mapping(serde_yaml::Mapping::new()),
            env_enabled: false,
        }
    }

    /// Resolve a dotted path expression.
    ///
    /// Resolution order:
    /// 1. If the path starts with `env.`, resolve from the process
    ///    environment (only when `env_enabled` is true).
    /// 2. Look up in `params` (higher precedence).
    /// 3. Fall back to `vars`.
    ///
    /// Returns the scalar value as a string. Returns an error if the
    /// key is not found in any source.
    pub fn resolve(&self, expr: &str) -> Result<String> {
        let value = self.resolve_value(expr)?;
        value_to_string(&value, expr)
    }

    /// Resolve a dotted path expression to its underlying YAML value.
    ///
    /// This is used by command templating, where mappings can be
    /// rendered as CLI flags instead of rejected as non-scalar values.
    pub fn resolve_value(&self, expr: &str) -> Result<Value> {
        // Environment variable access: env.VAR_NAME
        if let Some(var_name) = expr.strip_prefix("env.") {
            if !self.env_enabled {
                return Err(WorkflowError::TemplateUndefined {
                    key: expr.to_owned(),
                    field: String::new(),
                    stage: String::new(),
                });
            }
            return match std::env::var(var_name) {
                Ok(val) => Ok(Value::String(val)),
                Err(_) => Err(WorkflowError::TemplateUndefined {
                    key: expr.to_owned(),
                    field: String::new(),
                    stage: String::new(),
                }),
            };
        }

        // Try params first (higher precedence), then vars.
        if let Some(val) = walk_path(&self.params, expr) {
            return Ok(val.clone());
        }
        if let Some(val) = walk_path(&self.vars, expr) {
            return Ok(val.clone());
        }

        Err(WorkflowError::TemplateUndefined {
            key: expr.to_owned(),
            field: String::new(),
            stage: String::new(),
        })
    }

    /// Load vars from a list of inline maps and/or params-file references.
    ///
    /// Each entry in `sources` is either:
    /// - A `Value::Mapping` (inline key-value pairs)
    /// - A `Value::Mapping` file selector (`params.yaml: [key]`)
    /// - A `Value::String` (path to a YAML/JSON/TOML/Python params file to load)
    ///
    /// Values are merged left-to-right; later entries override earlier
    /// ones on key conflicts.
    pub fn load_vars(sources: &[Value], base_dir: &Path) -> Result<Value> {
        let mut merged = serde_yaml::Mapping::new();

        for source in sources {
            match source {
                Value::Mapping(map) => {
                    if let Some((file_path, sections)) = vars_file_ref_from_mapping(map)? {
                        merge_vars_file(&mut merged, base_dir, &file_path, &sections)?;
                    } else {
                        for (k, v) in map {
                            merge_var_value(&mut merged, k.clone(), v.clone());
                        }
                    }
                }
                Value::String(path_str) => {
                    // Handle selective imports: "path:section1,section2"
                    let (file_path, sections) = parse_file_ref(path_str);
                    merge_vars_file(&mut merged, base_dir, file_path, &sections)?;
                }
                _ => {
                    return Err(WorkflowError::TemplateInvalid {
                        key: "vars entry must be a mapping or a file path string".into(),
                        origin: "template".into(),
                    });
                }
            }
        }

        Ok(Value::Mapping(merged))
    }

    /// Load params from declared param files, merging them into a
    /// single `Value::Mapping`.
    ///
    /// Files are loaded from the filesystem relative to `base_dir`.
    /// Later files override earlier ones on key conflicts.
    pub fn load_params(paths: &[std::path::PathBuf], base_dir: &Path) -> Result<Value> {
        let mut merged = serde_yaml::Mapping::new();

        for path in paths {
            let full_path = base_dir.join(path);
            if !full_path.is_file() {
                // Skip missing param files gracefully — they may not
                // exist yet (e.g. first run before params.yaml is created).
                continue;
            }
            let content = std::fs::read_to_string(&full_path).map_err(|e| {
                WorkflowError::TemplateInvalid {
                    key: format!("params file '{}': {e}", full_path.display()),
                    origin: "template".into(),
                }
            })?;
            let value = parse_params_file(&content, &full_path)?;
            if let Value::Mapping(map) = value {
                for (k, v) in map {
                    merged.insert(k, v);
                }
            }
        }

        Ok(Value::Mapping(merged))
    }

    /// Whether env access is enabled.
    pub fn env_enabled(&self) -> bool {
        self.env_enabled
    }

    /// Borrow the vars value tree (used by foreach expansion to merge contexts).
    pub fn vars_value(&self) -> &Value {
        &self.vars
    }

    /// Borrow the params value tree (used by foreach expansion to merge contexts).
    pub fn params_value(&self) -> &Value {
        &self.params
    }
}

fn merge_vars_file(
    merged: &mut serde_yaml::Mapping,
    base_dir: &Path,
    file_path: &str,
    sections: &[String],
) -> Result<()> {
    let full_path = base_dir.join(file_path);
    let content =
        std::fs::read_to_string(&full_path).map_err(|e| WorkflowError::TemplateInvalid {
            key: format!("vars file '{}': {e}", full_path.display()),
            origin: "template".into(),
        })?;
    let file_value = parse_params_file(&content, &full_path)?;

    if let Value::Mapping(file_map) = file_value {
        if sections.is_empty() {
            for (k, v) in file_map {
                merge_var_value(merged, k, v);
            }
        } else {
            for section in sections {
                let key = Value::String(section.clone());
                if let Some(v) = file_map.get(&key) {
                    merge_var_value(merged, key, v.clone());
                }
            }
        }
    }

    Ok(())
}

fn vars_file_ref_from_mapping(map: &serde_yaml::Mapping) -> Result<Option<(String, Vec<String>)>> {
    if map.len() != 1 {
        return Ok(None);
    }

    let Some((Value::String(key), value)) = map.iter().next() else {
        return Ok(None);
    };
    if !looks_like_vars_file_ref(key) {
        return Ok(None);
    }

    let (file_path, key_sections) = parse_file_ref(key);
    let value_sections = selectors_from_vars_file_mapping_value(value)?;
    if !key_sections.is_empty() && !value_sections.is_empty() {
        return Err(WorkflowError::TemplateInvalid {
            key: format!("vars file selector '{key}' cannot also specify selector values"),
            origin: "template".into(),
        });
    }

    let sections = if key_sections.is_empty() {
        value_sections
    } else {
        key_sections
    };
    Ok(Some((file_path.to_owned(), sections)))
}

fn looks_like_vars_file_ref(input: &str) -> bool {
    let (file_path, _) = parse_file_ref(input);
    let Some(ext) = Path::new(file_path).extension().and_then(|e| e.to_str()) else {
        return false;
    };
    matches!(
        ext.to_ascii_lowercase().as_str(),
        "yaml" | "yml" | "json" | "toml" | "py"
    )
}

fn selectors_from_vars_file_mapping_value(value: &Value) -> Result<Vec<String>> {
    match value {
        Value::Null => Ok(Vec::new()),
        Value::String(selector) => Ok(parse_selector_text(selector)),
        Value::Sequence(selectors) => {
            let mut parsed = Vec::new();
            for selector in selectors {
                match selector {
                    Value::String(value) => parsed.extend(parse_selector_text(value)),
                    Value::Tagged(tagged) => {
                        parsed.extend(selectors_from_vars_file_mapping_value(&tagged.value)?);
                    }
                    _ => {
                        return Err(WorkflowError::TemplateInvalid {
                            key: "vars file selector entries must be strings".into(),
                            origin: "template".into(),
                        });
                    }
                }
            }
            Ok(parsed)
        }
        Value::Tagged(tagged) => selectors_from_vars_file_mapping_value(&tagged.value),
        _ => Err(WorkflowError::TemplateInvalid {
            key: "vars file selector must be null, a string, or a list of strings".into(),
            origin: "template".into(),
        }),
    }
}

fn parse_selector_text(selector: &str) -> Vec<String> {
    selector
        .split(',')
        .map(str::trim)
        .filter(|section| !section.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn merge_var_value(map: &mut serde_yaml::Mapping, key: Value, value: Value) {
    match (map.get_mut(&key), value) {
        (Some(Value::Mapping(existing)), Value::Mapping(incoming)) => {
            merge_var_mapping(existing, incoming);
        }
        (Some(existing), incoming) => {
            *existing = incoming;
        }
        (None, incoming) => {
            map.insert(key, incoming);
        }
    }
}

fn merge_var_mapping(target: &mut serde_yaml::Mapping, incoming: serde_yaml::Mapping) {
    for (key, value) in incoming {
        merge_var_value(target, key, value);
    }
}

/// Walk a dotted path through a nested `Value` tree.
///
/// Supports:
/// - `key` — top-level lookup
/// - `key.nested.path` — nested mapping traversal
/// - `key.0` — sequence index access (numeric segments)
fn walk_path<'v>(root: &'v Value, path: &str) -> Option<&'v Value> {
    let segments: Vec<&str> = path.split('.').collect();
    let mut current = root;

    for segment in &segments {
        match current {
            Value::Mapping(map) => {
                let key = Value::String((*segment).to_owned());
                current = map.get(&key)?;
            }
            Value::Sequence(seq) => {
                // Try numeric index access.
                let idx: usize = segment.parse().ok()?;
                current = seq.get(idx)?;
            }
            _ => return None,
        }
    }

    Some(current)
}

/// Convert a YAML `Value` leaf to its string representation.
fn value_to_string(val: &Value, expr: &str) -> Result<String> {
    match val {
        Value::Null => Ok("null".to_owned()),
        Value::Bool(b) => Ok(b.to_string()),
        Value::Number(n) => Ok(n.to_string()),
        Value::String(s) => Ok(s.clone()),
        Value::Sequence(_) | Value::Mapping(_) => Err(WorkflowError::TemplateInvalid {
            key: format!(
                "template expression '{expr}' resolves to a complex value (mapping or sequence), not a scalar"
            ),
            origin: "template".into(),
        }),
        Value::Tagged(t) => value_to_string(&t.value, expr),
    }
}

/// Parse a vars file reference, splitting off optional section selectors.
///
/// Format: `"path/to/file.yaml"` or `"path/to/file.yaml:section1,section2"`
fn parse_file_ref(input: &str) -> (&str, Vec<String>) {
    match input.split_once(':') {
        Some((path, sections_str)) => {
            let sections: Vec<String> = sections_str
                .split(',')
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
                .collect();
            (path, sections)
        }
        None => (input, Vec::new()),
    }
}

/// Parse a file as a YAML value (the primary format for vars/params files).
///
/// Falls back to JSON or TOML based on extension.
fn parse_vars_file(content: &str, path: &Path) -> Result<Value> {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("yaml");

    match ext {
        "yaml" | "yml" => {
            serde_yaml::from_str(content).map_err(|e| WorkflowError::TemplateInvalid {
                key: format!("failed to parse '{}': {e}", path.display()),
                origin: "template".into(),
            })
        }
        "json" => {
            let json_val: serde_json::Value =
                serde_json::from_str(content).map_err(|e| WorkflowError::TemplateInvalid {
                    key: format!("failed to parse '{}': {e}", path.display()),
                    origin: "template".into(),
                })?;
            // Convert JSON to YAML value for uniform handling.
            let yaml_str =
                serde_yaml::to_string(&json_val).map_err(|e| WorkflowError::TemplateInvalid {
                    key: format!("failed to convert '{}' to YAML: {e}", path.display()),
                    origin: "template".into(),
                })?;
            serde_yaml::from_str(&yaml_str).map_err(|e| WorkflowError::TemplateInvalid {
                key: format!("failed to re-parse '{}': {e}", path.display()),
                origin: "template".into(),
            })
        }
        "toml" => {
            let toml_val: toml::Value =
                toml::from_str(content).map_err(|e| WorkflowError::TemplateInvalid {
                    key: format!("failed to parse '{}': {e}", path.display()),
                    origin: "template".into(),
                })?;
            // Convert TOML to YAML value for uniform handling.
            let yaml_str =
                serde_yaml::to_string(&toml_val).map_err(|e| WorkflowError::TemplateInvalid {
                    key: format!("failed to convert '{}' to YAML: {e}", path.display()),
                    origin: "template".into(),
                })?;
            serde_yaml::from_str(&yaml_str).map_err(|e| WorkflowError::TemplateInvalid {
                key: format!("failed to re-parse '{}': {e}", path.display()),
                origin: "template".into(),
            })
        }
        other => Err(WorkflowError::TemplateInvalid {
            key: format!(
                "unsupported vars file extension '.{other}' for '{}' (expected .yaml, .yml, .json, or .toml)",
                path.display()
            ),
            origin: "template".into(),
        }),
    }
}

/// Parse a declared params file for template resolution.
///
/// DVC treats `vars:` file references as parameter files for
/// templating, so Python literal params must work there too.
fn parse_params_file(content: &str, path: &Path) -> Result<Value> {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("yaml");
    match ext {
        "yaml" | "yml" | "json" | "toml" => parse_vars_file(content, path),
        "py" => parse_python_params_file(content, path),
        other => Err(WorkflowError::TemplateInvalid {
            key: format!(
                "unsupported params file extension '.{other}' for '{}' (expected .yaml, .yml, .json, .toml, or .py)",
                path.display()
            ),
            origin: "template".into(),
        }),
    }
}

fn parse_python_params_file(content: &str, path: &Path) -> Result<Value> {
    let flattened = params::parse_python(content)?;
    let mut root = serde_yaml::Mapping::new();

    for (key, scalar) in flattened {
        let value = scalar_to_yaml_value(scalar, path, &key)?;
        insert_dotted_value(&mut root, &key, value);
    }

    Ok(Value::Mapping(root))
}

fn scalar_to_yaml_value(scalar: Scalar, path: &Path, key: &str) -> Result<Value> {
    serde_yaml::to_value(scalar).map_err(|e| WorkflowError::TemplateInvalid {
        key: format!(
            "failed to convert Python param '{}:{key}' to template value: {e}",
            path.display()
        ),
        origin: "template".into(),
    })
}

fn insert_dotted_value(map: &mut serde_yaml::Mapping, key: &str, value: Value) {
    let Some((head, tail)) = key.split_once('.') else {
        map.insert(Value::String(key.to_owned()), value);
        return;
    };

    let head_key = Value::String(head.to_owned());
    let entry = map
        .entry(head_key)
        .or_insert_with(|| Value::Mapping(serde_yaml::Mapping::new()));
    if !matches!(entry, Value::Mapping(_)) {
        *entry = Value::Mapping(serde_yaml::Mapping::new());
    }
    if let Value::Mapping(child) = entry {
        insert_dotted_value(child, tail, value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn resolve_simple_key() {
        let vars = serde_yaml::from_str("codedir: src").unwrap();
        let params = Value::Mapping(serde_yaml::Mapping::new());
        let ctx = TemplateContext::new(vars, params, false);

        assert_eq!(ctx.resolve("codedir").unwrap(), "src");
    }

    #[test]
    fn resolve_nested_key() {
        let params = serde_yaml::from_str("model:\n  lr: 0.001\n  epochs: 10").unwrap();
        let vars = Value::Mapping(serde_yaml::Mapping::new());
        let ctx = TemplateContext::new(vars, params, false);

        assert_eq!(ctx.resolve("model.lr").unwrap(), "0.001");
        assert_eq!(ctx.resolve("model.epochs").unwrap(), "10");
    }

    #[test]
    fn params_override_vars() {
        let vars = serde_yaml::from_str("lr: 0.1").unwrap();
        let params = serde_yaml::from_str("lr: 0.001").unwrap();
        let ctx = TemplateContext::new(vars, params, false);

        assert_eq!(ctx.resolve("lr").unwrap(), "0.001");
    }

    #[test]
    fn undefined_key_returns_error() {
        let ctx = TemplateContext::empty();
        let err = ctx.resolve("nonexistent").unwrap_err();
        assert!(matches!(err, WorkflowError::TemplateUndefined { .. }));
    }

    #[test]
    fn env_access_disabled_by_default() {
        let ctx = TemplateContext::empty();
        let err = ctx.resolve("env.HOME").unwrap_err();
        assert!(matches!(err, WorkflowError::TemplateUndefined { .. }));
    }

    #[test]
    fn env_access_when_enabled() {
        // SAFETY: This test runs single-threaded and the env var is
        // unique to this test, so no other thread reads it concurrently.
        unsafe {
            std::env::set_var("CRAB_TEST_TEMPLATE_VAR", "hello");
        }
        let ctx = TemplateContext::new(
            Value::Mapping(serde_yaml::Mapping::new()),
            Value::Mapping(serde_yaml::Mapping::new()),
            true,
        );

        assert_eq!(ctx.resolve("env.CRAB_TEST_TEMPLATE_VAR").unwrap(), "hello");
        unsafe {
            std::env::remove_var("CRAB_TEST_TEMPLATE_VAR");
        }
    }

    #[test]
    fn resolve_sequence_index() {
        let params = serde_yaml::from_str("widths:\n  - 64\n  - 128\n  - 256").unwrap();
        let vars = Value::Mapping(serde_yaml::Mapping::new());
        let ctx = TemplateContext::new(vars, params, false);

        assert_eq!(ctx.resolve("widths.0").unwrap(), "64");
        assert_eq!(ctx.resolve("widths.2").unwrap(), "256");
    }

    #[test]
    fn resolve_bool_and_null() {
        let params = serde_yaml::from_str("debug: true\nnothing: null").unwrap();
        let vars = Value::Mapping(serde_yaml::Mapping::new());
        let ctx = TemplateContext::new(vars, params, false);

        assert_eq!(ctx.resolve("debug").unwrap(), "true");
        assert_eq!(ctx.resolve("nothing").unwrap(), "null");
    }

    #[test]
    fn complex_value_returns_error() {
        let params = serde_yaml::from_str("model:\n  lr: 0.001\n  epochs: 10").unwrap();
        let vars = Value::Mapping(serde_yaml::Mapping::new());
        let ctx = TemplateContext::new(vars, params, false);

        // Resolving a mapping (not a leaf) should error.
        let err = ctx.resolve("model").unwrap_err();
        assert!(matches!(err, WorkflowError::TemplateInvalid { .. }));
    }

    #[test]
    fn load_vars_inline_maps() {
        let sources = vec![
            serde_yaml::from_str::<Value>("codedir: src").unwrap(),
            serde_yaml::from_str::<Value>("datadir: data").unwrap(),
        ];
        let merged = TemplateContext::load_vars(&sources, Path::new(".")).unwrap();
        let ctx = TemplateContext::new(merged, Value::Mapping(serde_yaml::Mapping::new()), false);

        assert_eq!(ctx.resolve("codedir").unwrap(), "src");
        assert_eq!(ctx.resolve("datadir").unwrap(), "data");
    }

    #[test]
    fn load_vars_later_overrides_earlier() {
        let sources = vec![
            serde_yaml::from_str::<Value>("x: first").unwrap(),
            serde_yaml::from_str::<Value>("x: second").unwrap(),
        ];
        let merged = TemplateContext::load_vars(&sources, Path::new(".")).unwrap();
        let ctx = TemplateContext::new(merged, Value::Mapping(serde_yaml::Mapping::new()), false);

        assert_eq!(ctx.resolve("x").unwrap(), "second");
    }

    #[test]
    fn load_vars_deep_merges_non_conflicting_maps() {
        let sources = vec![
            serde_yaml::from_str::<Value>("grp:\n  a: 1").unwrap(),
            serde_yaml::from_str::<Value>("grp:\n  b: 2").unwrap(),
        ];
        let merged = TemplateContext::load_vars(&sources, Path::new(".")).unwrap();
        let ctx = TemplateContext::new(merged, Value::Mapping(serde_yaml::Mapping::new()), false);

        assert_eq!(ctx.resolve("grp.a").unwrap(), "1");
        assert_eq!(ctx.resolve("grp.b").unwrap(), "2");
    }

    #[test]
    fn load_vars_mapping_file_selector_imports_selected_keys() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("params.yaml"),
            "clean:\n  filename: data/clean.csv\nfeats:\n  dirname: features\ntrain:\n  epochs: 5\n",
        )
        .unwrap();
        let sources =
            vec![serde_yaml::from_str::<Value>("params.yaml:\n  - clean\n  - feats").unwrap()];

        let merged = TemplateContext::load_vars(&sources, tmp.path()).unwrap();
        let ctx = TemplateContext::new(merged, Value::Mapping(serde_yaml::Mapping::new()), false);

        assert_eq!(ctx.resolve("clean.filename").unwrap(), "data/clean.csv");
        assert_eq!(ctx.resolve("feats.dirname").unwrap(), "features");
        assert!(matches!(
            ctx.resolve("train.epochs"),
            Err(WorkflowError::TemplateUndefined { .. })
        ));
    }

    #[test]
    fn load_vars_accepts_python_params_files() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("params.py"),
            "lr = 0.01\nclass Train:\n    epochs = 5\n",
        )
        .unwrap();
        let sources = vec![Value::String("params.py".to_owned())];

        let merged = TemplateContext::load_vars(&sources, tmp.path()).unwrap();
        let ctx = TemplateContext::new(merged, Value::Mapping(serde_yaml::Mapping::new()), false);

        assert_eq!(ctx.resolve("lr").unwrap(), "0.01");
        assert_eq!(ctx.resolve("Train.epochs").unwrap(), "5");
    }

    #[test]
    fn load_vars_selects_python_params_keys() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("params.py"), "lr = 0.01\nmomentum = 0.9\n").unwrap();
        let sources = vec![Value::String("params.py:lr".to_owned())];

        let merged = TemplateContext::load_vars(&sources, tmp.path()).unwrap();
        let ctx = TemplateContext::new(merged, Value::Mapping(serde_yaml::Mapping::new()), false);

        assert_eq!(ctx.resolve("lr").unwrap(), "0.01");
        assert!(matches!(
            ctx.resolve("momentum"),
            Err(WorkflowError::TemplateUndefined { .. })
        ));
    }

    #[test]
    fn load_params_accepts_python_literal_assignments() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("params.py"),
            "model = {'lr': 0.01}\nwidths = [64, 128]\nflag = True\n",
        )
        .unwrap();

        let params =
            TemplateContext::load_params(&[PathBuf::from("params.py")], tmp.path()).unwrap();
        let ctx = TemplateContext::new(Value::Mapping(serde_yaml::Mapping::new()), params, false);

        assert_eq!(ctx.resolve("model.lr").unwrap(), "0.01");
        assert_eq!(ctx.resolve("widths.1").unwrap(), "128");
        assert_eq!(ctx.resolve("flag").unwrap(), "true");
    }

    #[test]
    fn parse_file_ref_no_sections() {
        let (path, sections) = parse_file_ref("config/extra.yaml");
        assert_eq!(path, "config/extra.yaml");
        assert!(sections.is_empty());
    }

    #[test]
    fn parse_file_ref_with_sections() {
        let (path, sections) = parse_file_ref("config/extra.yaml:section1,section2");
        assert_eq!(path, "config/extra.yaml");
        assert_eq!(sections, vec!["section1", "section2"]);
    }
}
