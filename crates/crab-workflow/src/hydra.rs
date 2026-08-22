//! Hydra-style config composition for experiment worktrees.
//!
//! DVC composes Hydra configs into `params.yaml` before running an
//! experiment. Crab mirrors the same product boundary: composition
//! happens inside the throwaway experiment tree, then normal
//! `--set-param` scalar overrides mutate the composed params file.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde_yaml::{Mapping, Value};

use crate::materialize::write_atomic;
use crate::{Result, WorkflowError as CrabError};

const CONFIG_REL: &str = ".crab/config.toml";
const DEFAULT_CONFIG_DIR: &str = "conf";
const DEFAULT_CONFIG_NAME: &str = "config.yaml";
const DEFAULT_OUTPUT: &str = "params.yaml";

/// Compose Hydra config into `params.yaml` when `[hydra] enabled = true`.
///
/// Returns the overrides that were not consumed as config-group/defaults-list
/// overrides so the caller can apply them as ordinary scalar param updates.
pub(crate) fn compose_if_enabled(
    tmpdir: &Path,
    overrides: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>> {
    let Some(config) = HydraConfig::load(tmpdir)? else {
        return Ok(overrides.clone());
    };

    let composer = Composer::new(tmpdir, config);
    let mut primary = composer.load_primary()?;
    let remaining = composer.apply_group_overrides(&mut primary.defaults, overrides)?;
    let mut output = composer.compose_from_parts(primary, Vec::new(), Vec::new())?;
    resolve_interpolations(&mut output)?;
    write_params_yaml(tmpdir, &output)?;
    Ok(remaining)
}

#[derive(Debug, Clone)]
struct HydraConfig {
    config_dir: PathBuf,
    config_name: PathBuf,
}

impl HydraConfig {
    fn load(tmpdir: &Path) -> Result<Option<Self>> {
        let config_path = tmpdir.join(CONFIG_REL);
        let text = match fs::read_to_string(&config_path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(CrabError::Io(e)),
        };
        let table = text
            .parse::<toml::Table>()
            .map_err(|e| CrabError::Configuration {
                key: format!("failed to parse {CONFIG_REL}: {e}"),
                origin: "hydra".to_owned(),
            })?;
        let Some(hydra) = table.get("hydra").and_then(toml::Value::as_table) else {
            return Ok(None);
        };
        let enabled = hydra
            .get("enabled")
            .and_then(toml::Value::as_bool)
            .unwrap_or(false);
        if !enabled {
            return Ok(None);
        }

        Ok(Some(Self {
            config_dir: PathBuf::from(
                hydra
                    .get("config_dir")
                    .and_then(toml::Value::as_str)
                    .unwrap_or(DEFAULT_CONFIG_DIR),
            ),
            config_name: PathBuf::from(
                hydra
                    .get("config_name")
                    .and_then(toml::Value::as_str)
                    .unwrap_or(DEFAULT_CONFIG_NAME),
            ),
        }))
    }
}

#[derive(Debug)]
struct ConfigParts {
    defaults: Vec<DefaultEntry>,
    body: Value,
    package: Option<String>,
}

#[derive(Debug, Clone)]
enum DefaultEntry {
    Self_,
    Placeholder {
        group: String,
        package: Option<String>,
        absolute: bool,
    },
    Config {
        request: ConfigRequest,
        optional: bool,
        override_existing: bool,
    },
}

#[derive(Debug, Clone)]
struct ConfigRequest {
    group: Option<String>,
    package: Option<String>,
    option: String,
    absolute: bool,
}

impl ConfigRequest {
    fn full_group_path(&self, relative_group: &[String]) -> Vec<String> {
        let Some(group) = &self.group else {
            if self.absolute {
                return Vec::new();
            }
            return relative_group.to_vec();
        };

        let mut path = if self.absolute {
            Vec::new()
        } else {
            relative_group.to_vec()
        };
        path.extend(split_path(group));
        path
    }

    fn package_path(&self, parent_package: &[String], relative_group: &[String]) -> Vec<String> {
        let group_package = self.full_group_path(relative_group);
        if let Some(package) = &self.package {
            return expand_package_override(package, parent_package, &group_package);
        }
        let Some(group) = &self.group else {
            if self.absolute {
                return Vec::new();
            }
            return parent_package.to_vec();
        };
        if self.absolute {
            return split_path(group);
        }
        let mut path = parent_package.to_vec();
        path.extend(split_path(group));
        path
    }

    fn load_path(&self, config_dir: &Path, relative_group: &[String]) -> PathBuf {
        let mut path = config_dir.to_path_buf();
        if let Some(group) = &self.group {
            if self.absolute {
                path.extend(split_path(group));
            } else {
                path.extend(relative_group);
                path.extend(split_path(group));
            }
            path.push(format!("{}.yaml", self.option));
        } else if self.absolute {
            path.push(format!("{}.yaml", self.option));
        } else {
            path.extend(relative_group);
            path.push(format!("{}.yaml", self.option));
        }
        path
    }
}

struct Composer {
    root: PathBuf,
    config: HydraConfig,
}

impl Composer {
    fn new(root: &Path, config: HydraConfig) -> Self {
        Self {
            root: root.to_path_buf(),
            config,
        }
    }

    fn config_dir(&self) -> PathBuf {
        self.root.join(&self.config.config_dir)
    }

    fn load_primary(&self) -> Result<ConfigParts> {
        let path = self.config_dir().join(&self.config.config_name);
        Self::load_parts(&path)
    }

    fn load_parts(path: &Path) -> Result<ConfigParts> {
        let text = fs::read_to_string(path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                CrabError::Configuration {
                    key: path.display().to_string(),
                    origin: "hydra config file not found".to_owned(),
                }
            } else {
                CrabError::Io(e)
            }
        })?;
        let package = parse_package_directive(&text, path)?;
        let mut value: Value =
            serde_yaml::from_str(&text).map_err(|e| CrabError::Configuration {
                key: format!("hydra config parse error in {}: {e}", path.display()),
                origin: "hydra".to_owned(),
            })?;
        let defaults = take_defaults(&mut value, path)?;
        Ok(ConfigParts {
            defaults,
            body: value,
            package,
        })
    }

    fn apply_group_overrides(
        &self,
        defaults: &mut Vec<DefaultEntry>,
        overrides: &BTreeMap<String, String>,
    ) -> Result<BTreeMap<String, String>> {
        let mut remaining = BTreeMap::new();
        for (key, value) in overrides {
            if self.apply_group_override(defaults, key, value)? {
                continue;
            }
            remaining.insert(key.clone(), value.clone());
        }
        Ok(remaining)
    }

    fn apply_group_override(
        &self,
        defaults: &mut Vec<DefaultEntry>,
        key: &str,
        value: &str,
    ) -> Result<bool> {
        let (op, raw_target) = parse_override_op(key);
        let Some(target) = GroupOverrideTarget::parse(raw_target) else {
            return Ok(false);
        };
        if !target.group.contains('/')
            && !defaults
                .iter()
                .any(|entry| entry_matches_target(entry, &target))
            && !self.group_option_exists(&target.group, value)
            && !matches!(op, OverrideOp::Remove)
        {
            return Ok(false);
        }

        match op {
            OverrideOp::SetExisting => {
                for entry in defaults.iter_mut().rev() {
                    if !entry_matches_target(entry, &target) {
                        continue;
                    }
                    match entry {
                        DefaultEntry::Config { request, .. } => {
                            value.clone_into(&mut request.option);
                        }
                        DefaultEntry::Placeholder {
                            group,
                            package,
                            absolute,
                        } => {
                            *entry = DefaultEntry::Config {
                                request: ConfigRequest {
                                    group: Some(group.clone()),
                                    package: package.clone(),
                                    option: value.to_owned(),
                                    absolute: *absolute,
                                },
                                optional: false,
                                override_existing: false,
                            };
                        }
                        DefaultEntry::Self_ => {}
                    }
                    return Ok(true);
                }
                if self.group_option_exists(&target.group, value) {
                    defaults.push(DefaultEntry::Config {
                        request: ConfigRequest {
                            group: Some(target.group),
                            package: target.package,
                            option: value.to_owned(),
                            absolute: false,
                        },
                        optional: false,
                        override_existing: false,
                    });
                    return Ok(true);
                }
                Ok(false)
            }
            OverrideOp::Add | OverrideOp::AddOrSet => {
                if matches!(op, OverrideOp::AddOrSet) {
                    defaults.retain(|entry| !entry_matches_target(entry, &target));
                } else if defaults
                    .iter()
                    .any(|entry| entry_matches_target(entry, &target))
                {
                    return Err(CrabError::Configuration {
                        key: format!("hydra config group already selected: {}", target.raw()),
                        origin: "hydra".to_owned(),
                    });
                }
                defaults.push(DefaultEntry::Config {
                    request: ConfigRequest {
                        group: Some(target.group),
                        package: target.package,
                        option: value.to_owned(),
                        absolute: false,
                    },
                    optional: false,
                    override_existing: false,
                });
                Ok(true)
            }
            OverrideOp::Remove => {
                let before = defaults.len();
                defaults.retain(|entry| !entry_matches_target(entry, &target));
                Ok(defaults.len() != before)
            }
        }
    }

    fn group_option_exists(&self, group: &str, option: &str) -> bool {
        self.config_dir()
            .join(group)
            .join(format!("{option}.yaml"))
            .is_file()
    }

    fn compose_from_parts(
        &self,
        parts: ConfigParts,
        relative_group: Vec<String>,
        package: Vec<String>,
    ) -> Result<Value> {
        let ConfigParts { defaults, body, .. } = parts;
        let mut result = Value::Mapping(Mapping::new());
        let mut has_self = false;
        let mut ordered = normalize_defaults(defaults);
        if !ordered
            .iter()
            .any(|entry| matches!(entry, DefaultEntry::Self_))
        {
            ordered.push(DefaultEntry::Self_);
        }

        for entry in ordered {
            match entry {
                DefaultEntry::Self_ => {
                    has_self = true;
                    merge_value(&mut result, package_value(body.clone(), &package));
                }
                DefaultEntry::Placeholder { .. } => {}
                DefaultEntry::Config {
                    request, optional, ..
                } => {
                    let path = request.load_path(&self.config_dir(), &relative_group);
                    if optional && !path.is_file() {
                        continue;
                    }
                    let child_parts = Self::load_parts(&path)?;
                    let child_group = request.full_group_path(&relative_group);
                    let child_package = request_package(
                        &request,
                        child_parts.package.as_deref(),
                        &package,
                        &relative_group,
                    )?;
                    let child = self.compose_from_parts(child_parts, child_group, child_package)?;
                    merge_value(&mut result, child);
                }
            }
        }

        if !has_self {
            merge_value(&mut result, package_value(body, &package));
        }
        Ok(result)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OverrideOp {
    SetExisting,
    Add,
    AddOrSet,
    Remove,
}

#[derive(Debug)]
struct GroupOverrideTarget {
    group: String,
    package: Option<String>,
}

impl GroupOverrideTarget {
    fn parse(raw: &str) -> Option<Self> {
        if raw.is_empty() || raw.contains(':') {
            return None;
        }
        let raw = raw.trim_start_matches('/');
        let (group, package) = split_group_package(raw);
        if group.is_empty() || group.contains('.') {
            return None;
        }
        Some(Self {
            group: group.to_owned(),
            package: package.map(str::to_owned),
        })
    }

    fn raw(&self) -> String {
        self.package.as_ref().map_or_else(
            || self.group.clone(),
            |package| format!("{}@{package}", self.group),
        )
    }
}

fn parse_override_op(key: &str) -> (OverrideOp, &str) {
    if let Some(rest) = key.strip_prefix("++") {
        (OverrideOp::AddOrSet, rest)
    } else if let Some(rest) = key.strip_prefix('+') {
        (OverrideOp::Add, rest)
    } else if let Some(rest) = key.strip_prefix('~') {
        (OverrideOp::Remove, rest)
    } else {
        (OverrideOp::SetExisting, key)
    }
}

fn normalize_defaults(defaults: Vec<DefaultEntry>) -> Vec<DefaultEntry> {
    let mut normalized = Vec::with_capacity(defaults.len());
    for entry in defaults {
        match entry {
            DefaultEntry::Config {
                request,
                optional,
                override_existing,
            } => {
                if override_existing {
                    normalized.retain(|existing| !entry_matches_request(existing, &request));
                }
                normalized.push(DefaultEntry::Config {
                    request,
                    optional,
                    override_existing: false,
                });
            }
            DefaultEntry::Placeholder { .. } | DefaultEntry::Self_ => {
                normalized.push(entry);
            }
        }
    }
    normalized
}

fn entry_matches_target(entry: &DefaultEntry, target: &GroupOverrideTarget) -> bool {
    match entry {
        DefaultEntry::Config { request, .. } => {
            request.group.as_deref() == Some(target.group.as_str())
                && request.package.as_deref() == target.package.as_deref()
        }
        DefaultEntry::Placeholder { group, package, .. } => {
            group == &target.group && package.as_deref() == target.package.as_deref()
        }
        DefaultEntry::Self_ => false,
    }
}

fn entry_matches_request(entry: &DefaultEntry, request: &ConfigRequest) -> bool {
    match entry {
        DefaultEntry::Config {
            request: existing, ..
        } => {
            existing.group == request.group
                && existing.package == request.package
                && existing.absolute == request.absolute
        }
        DefaultEntry::Placeholder {
            group,
            package,
            absolute,
        } => {
            request.group.as_deref() == Some(group.as_str())
                && request.package.as_deref() == package.as_deref()
                && request.absolute == *absolute
        }
        DefaultEntry::Self_ => false,
    }
}

fn parse_package_directive(text: &str, path: &Path) -> Result<Option<String>> {
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.is_empty() {
            continue;
        }
        let Some(comment) = trimmed.strip_prefix('#') else {
            return Ok(None);
        };
        let comment = comment.trim_start();
        let Some(package) = comment.strip_prefix("@package") else {
            continue;
        };
        let package = package.trim();
        if package.is_empty() {
            return Err(CrabError::Configuration {
                key: format!("empty hydra package directive in {}", path.display()),
                origin: "hydra".to_owned(),
            });
        }
        return Ok(Some(package.to_owned()));
    }
    Ok(None)
}

fn take_defaults(value: &mut Value, path: &Path) -> Result<Vec<DefaultEntry>> {
    let Value::Mapping(map) = value else {
        return Ok(Vec::new());
    };
    let raw_defaults = map.remove(Value::String("defaults".to_owned()));
    let Some(raw_defaults) = raw_defaults else {
        return Ok(Vec::new());
    };
    let Value::Sequence(items) = raw_defaults else {
        return Err(CrabError::Configuration {
            key: format!("defaults in {} must be a list", path.display()),
            origin: "hydra".to_owned(),
        });
    };
    let mut defaults = Vec::new();
    for item in items {
        defaults.extend(parse_default_entry(&item, path)?);
    }
    Ok(defaults)
}

fn parse_default_entry(item: &Value, path: &Path) -> Result<Vec<DefaultEntry>> {
    match item {
        Value::String(raw) => parse_default_string(raw, path),
        Value::Mapping(map) => parse_default_mapping(map, path),
        _ => Err(CrabError::Configuration {
            key: format!("unsupported hydra defaults entry in {}", path.display()),
            origin: "hydra".to_owned(),
        }),
    }
}

fn parse_default_string(raw: &str, path: &Path) -> Result<Vec<DefaultEntry>> {
    let (flags, raw) = strip_default_flags(raw);
    if raw == "_self_" {
        return Ok(vec![DefaultEntry::Self_]);
    }
    let absolute = raw.starts_with('/');
    let raw = raw.trim_start_matches('/');
    let (group, package, option) = parse_config_path(raw);
    if option.is_empty() {
        return Err(CrabError::Configuration {
            key: format!("empty hydra defaults entry in {}", path.display()),
            origin: "hydra".to_owned(),
        });
    }
    Ok(vec![DefaultEntry::Config {
        request: ConfigRequest {
            group: group.map(str::to_owned),
            package: package.map(str::to_owned),
            option: option.to_owned(),
            absolute,
        },
        optional: flags.optional,
        override_existing: flags.override_existing,
    }])
}

fn parse_default_mapping(map: &Mapping, path: &Path) -> Result<Vec<DefaultEntry>> {
    if map.len() != 1 {
        return Err(CrabError::Configuration {
            key: format!(
                "hydra defaults mapping in {} must have one key",
                path.display()
            ),
            origin: "hydra".to_owned(),
        });
    }
    let Some((key, value)) = map.iter().next() else {
        return Err(CrabError::Configuration {
            key: format!("empty hydra defaults mapping in {}", path.display()),
            origin: "hydra".to_owned(),
        });
    };
    let key = key.as_str().ok_or_else(|| CrabError::Configuration {
        key: format!("hydra defaults key in {} must be a string", path.display()),
        origin: "hydra".to_owned(),
    })?;
    let (flags, key) = strip_default_flags(key);
    let absolute = key.starts_with('/');
    let key = key.trim_start_matches('/');
    let (group, package) = split_group_package(key);

    match value {
        Value::Null => Ok(vec![DefaultEntry::Placeholder {
            group: group.to_owned(),
            package: package.map(str::to_owned),
            absolute,
        }]),
        Value::String(option) => Ok(vec![DefaultEntry::Config {
            request: ConfigRequest {
                group: Some(group.to_owned()),
                package: package.map(str::to_owned),
                option: option.to_owned(),
                absolute,
            },
            optional: flags.optional,
            override_existing: flags.override_existing,
        }]),
        Value::Sequence(items) => items
            .iter()
            .map(|item| {
                let option = item.as_str().ok_or_else(|| CrabError::Configuration {
                    key: format!(
                        "hydra defaults list value for {key} in {} must contain strings",
                        path.display()
                    ),
                    origin: "hydra".to_owned(),
                })?;
                Ok(DefaultEntry::Config {
                    request: ConfigRequest {
                        group: Some(group.to_owned()),
                        package: package.map(str::to_owned),
                        option: option.to_owned(),
                        absolute,
                    },
                    optional: flags.optional,
                    override_existing: flags.override_existing,
                })
            })
            .collect(),
        _ => Err(CrabError::Configuration {
            key: format!(
                "hydra defaults value for {key} in {} must be a string, list, or null",
                path.display()
            ),
            origin: "hydra".to_owned(),
        }),
    }
}

#[derive(Default)]
struct DefaultFlags {
    optional: bool,
    override_existing: bool,
}

fn strip_default_flags(raw: &str) -> (DefaultFlags, &str) {
    let mut flags = DefaultFlags::default();
    let mut rest = raw.trim_start();
    loop {
        if let Some(next) = rest.strip_prefix("optional ") {
            flags.optional = true;
            rest = next.trim_start();
        } else if let Some(next) = rest.strip_prefix("override ") {
            flags.override_existing = true;
            rest = next.trim_start();
        } else {
            return (flags, rest);
        }
    }
}

fn split_group_package(raw: &str) -> (&str, Option<&str>) {
    raw.split_once('@')
        .map_or((raw, None), |(group, package)| (group, Some(package)))
}

fn parse_config_path(raw: &str) -> (Option<&str>, Option<&str>, &str) {
    let (path, package) = split_group_package(raw);
    match path.rsplit_once('/') {
        Some((group, option)) => (Some(group), package, option),
        None => (None, package, path),
    }
}

fn split_path(raw: &str) -> Vec<String> {
    raw.split('/')
        .filter(|seg| !seg.is_empty())
        .map(str::to_owned)
        .collect()
}

fn split_package(raw: &str) -> Vec<String> {
    raw.split(['.', '/'])
        .filter(|seg| !seg.is_empty())
        .map(str::to_owned)
        .collect()
}

fn request_package(
    request: &ConfigRequest,
    package_directive: Option<&str>,
    parent_package: &[String],
    relative_group: &[String],
) -> Result<Vec<String>> {
    if request.package.is_some() {
        return Ok(request.package_path(parent_package, relative_group));
    }
    if let Some(package) = package_directive {
        return Ok(expand_package_directive(
            package,
            &request.full_group_path(relative_group),
            &request.option,
        ));
    }
    Ok(request.package_path(parent_package, relative_group))
}

fn expand_package_override(
    package: &str,
    parent_package: &[String],
    default_package: &[String],
) -> Vec<String> {
    if package == "_group_" {
        return default_package.to_vec();
    }
    if let Some(rest) = package.strip_prefix("_group_.") {
        let mut path = default_package.to_vec();
        path.extend(split_package(rest));
        return path;
    }
    if package == "_global_" {
        return Vec::new();
    }
    if let Some(rest) = package.strip_prefix("_global_.") {
        return split_package(rest);
    }
    if package == "_here_" {
        return parent_package.to_vec();
    }
    if let Some(rest) = package.strip_prefix("_here_.") {
        let mut path = parent_package.to_vec();
        path.extend(split_package(rest));
        return path;
    }

    let mut path = parent_package.to_vec();
    path.extend(split_package(package));
    path
}

fn expand_package_directive(package: &str, default_group: &[String], name: &str) -> Vec<String> {
    let mut path = Vec::new();
    for segment in split_package(package) {
        match segment.as_str() {
            "_global_" => path.clear(),
            "_group_" => path.extend(default_group.iter().cloned()),
            "_name_" if !name.is_empty() => path.push(name.to_owned()),
            "_name_" => {}
            other => path.push(other.to_owned()),
        }
    }
    path
}

fn package_value(value: Value, package: &[String]) -> Value {
    if package.is_empty() {
        return value;
    }
    let mut out = value;
    for segment in package.iter().rev() {
        let mut map = Mapping::new();
        map.insert(Value::String(segment.clone()), out);
        out = Value::Mapping(map);
    }
    out
}

fn merge_value(dst: &mut Value, src: Value) {
    match (dst, src) {
        (Value::Mapping(dst_map), Value::Mapping(src_map)) => {
            for (key, value) in src_map {
                match dst_map.get_mut(&key) {
                    Some(existing) => merge_value(existing, value),
                    None => {
                        dst_map.insert(key, value);
                    }
                }
            }
        }
        (slot, value) => *slot = value,
    }
}

fn resolve_interpolations(root: &mut Value) -> Result<()> {
    for _ in 0..16 {
        let snapshot = root.clone();
        if !resolve_interpolations_once(root, &snapshot, &mut Vec::new())? {
            return Ok(());
        }
    }
    Err(CrabError::Configuration {
        key: "hydra interpolation".to_owned(),
        origin: "interpolation did not converge".to_owned(),
    })
}

fn resolve_interpolations_once(
    value: &mut Value,
    root: &Value,
    path: &mut Vec<String>,
) -> Result<bool> {
    match value {
        Value::String(text) => {
            let original = text.clone();
            if let Some(expr) = full_interpolation_expr(&original)
                && let Some(resolved) = resolve_interpolation_expr(expr, root, path)?
            {
                *value = resolved;
                return Ok(true);
            }
            resolve_string_interpolation(text, root, path)
        }
        Value::Sequence(items) => {
            let mut changed = false;
            for (index, item) in items.iter_mut().enumerate() {
                path.push(index.to_string());
                changed |= resolve_interpolations_once(item, root, path)?;
                path.pop();
            }
            Ok(changed)
        }
        Value::Mapping(map) => {
            let mut changed = false;
            for (key, item) in map.iter_mut() {
                if let Some(segment) = mapping_key_to_path_segment(key) {
                    path.push(segment);
                    changed |= resolve_interpolations_once(item, root, path)?;
                    path.pop();
                } else {
                    changed |= resolve_interpolations_once(item, root, path)?;
                }
            }
            Ok(changed)
        }
        _ => Ok(false),
    }
}

fn resolve_string_interpolation(text: &mut String, root: &Value, path: &[String]) -> Result<bool> {
    let original = text.clone();
    let mut out = String::new();
    let mut rest = original.as_str();
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after_start = &rest[start + 2..];
        let Some(end) = find_interpolation_end(after_start) else {
            return Ok(false);
        };
        let expr = &after_start[..end];
        if let Some(value) = resolve_interpolation_expr(expr, root, path)? {
            out.push_str(&scalar_to_string(&value)?);
        } else {
            out.push_str("${");
            out.push_str(expr);
            out.push('}');
        }
        rest = &after_start[end + 1..];
    }
    out.push_str(rest);
    if out != original {
        *text = out;
        return Ok(true);
    }
    Ok(false)
}

fn find_interpolation_end(input: &str) -> Option<usize> {
    let mut stack = Vec::new();
    let mut quote = None;
    let mut escaped = false;
    let bytes = input.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        let ch = input[i..].chars().next()?;
        if let Some(quote_ch) = quote {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == quote_ch {
                quote = None;
            }
            i += ch.len_utf8();
            continue;
        }

        if bytes[i] == b'$' && i + 1 < bytes.len() && bytes[i + 1] == b'{' {
            stack.push('}');
            i += 2;
            continue;
        }

        match ch {
            '\'' | '"' => quote = Some(ch),
            '(' => stack.push(')'),
            '[' => stack.push(']'),
            '{' => stack.push('}'),
            ')' | ']' | '}' => {
                if let Some(expected) = stack.pop() {
                    if ch != expected {
                        return None;
                    }
                } else if ch == '}' {
                    return Some(i);
                } else {
                    return None;
                }
            }
            _ => {}
        }
        i += ch.len_utf8();
    }
    None
}

fn resolve_interpolation_expr(expr: &str, root: &Value, path: &[String]) -> Result<Option<Value>> {
    if let Some((resolver, args)) = expr.split_once(':') {
        return resolve_resolver_expr(resolver.trim(), args, root, path);
    }
    Ok(lookup_config_path(root, expr, path).cloned())
}

fn resolve_resolver_expr(
    resolver: &str,
    args: &str,
    root: &Value,
    path: &[String],
) -> Result<Option<Value>> {
    match resolver {
        "join" => Ok(Some(Value::String(resolve_join(args, root, path)?))),
        "oc.create" => resolve_oc_create(args, root, path).map(Some),
        "oc.decode" => resolve_oc_decode(args, root, path).map(Some),
        "oc.deprecated" => resolve_oc_deprecated(args, root, path).map(Some),
        "oc.dict.keys" => resolve_oc_dict_keys(args, root, path).map(Some),
        "oc.dict.values" => resolve_oc_dict_values(args, root, path).map(Some),
        "oc.env" => resolve_oc_env(args, root, path).map(Some),
        "oc.select" => resolve_oc_select(args, root, path).map(Some),
        _ => Ok(None),
    }
}

fn resolve_join(args: &str, root: &Value, path: &[String]) -> Result<String> {
    let parts = split_resolver_args(args)?;
    if parts.is_empty() {
        return Err(CrabError::Configuration {
            key: "hydra resolver join".to_owned(),
            origin: "join requires at least one argument".to_owned(),
        });
    }

    let mut joined = PathBuf::new();
    for part in parts {
        let value = resolve_resolver_arg(&part, root, path)?;
        joined.push(value);
    }
    // Resolver values are serialized into portable YAML, so use `/` even
    // when composition runs on Windows.
    Ok(joined.to_string_lossy().replace('\\', "/"))
}

fn resolve_oc_env(args: &str, root: &Value, path: &[String]) -> Result<Value> {
    let parts = split_resolver_args(args)?;
    if !(1..=2).contains(&parts.len()) {
        return Err(CrabError::Configuration {
            key: "hydra resolver oc.env".to_owned(),
            origin: "oc.env requires one env var name and optional default".to_owned(),
        });
    }

    let name = unquote(&resolve_resolver_arg(&parts[0], root, path)?);
    match std::env::var(&name) {
        Ok(value) => Ok(Value::String(value)),
        Err(_) if parts.len() == 2 => {
            let default = resolve_resolver_arg(&parts[1], root, path)?;
            if default == "null" {
                Ok(Value::Null)
            } else {
                Ok(Value::String(default))
            }
        }
        Err(_) => Err(CrabError::Configuration {
            key: format!("hydra resolver oc.env: {name}"),
            origin: "environment variable is not set and no default was provided".to_owned(),
        }),
    }
}

fn resolve_oc_create(args: &str, root: &Value, path: &[String]) -> Result<Value> {
    resolve_yaml_node_resolver_arg("oc.create", args, root, path)
}

fn resolve_oc_decode(args: &str, root: &Value, path: &[String]) -> Result<Value> {
    resolve_yaml_node_resolver_arg("oc.decode", args, root, path)
}

fn resolve_yaml_node_resolver_arg(
    resolver: &str,
    args: &str,
    root: &Value,
    path: &[String],
) -> Result<Value> {
    let parts = split_resolver_args(args)?;
    if parts.len() != 1 {
        return Err(CrabError::Configuration {
            key: format!("hydra resolver {resolver}"),
            origin: format!("{resolver} requires exactly one argument"),
        });
    }

    let trimmed = parts[0].trim();
    if let Some(expr) = full_interpolation_expr(trimmed) {
        return match resolve_interpolation_expr(expr, root, path)? {
            Some(Value::String(value)) => decode_yaml_literal(resolver, &value),
            Some(Value::Null) => Ok(Value::Null),
            Some(value) => Ok(value),
            None => decode_yaml_literal(resolver, &unquote(trimmed)),
        };
    }

    let mut resolved = trimmed.to_owned();
    resolve_string_interpolation(&mut resolved, root, path)?;
    decode_yaml_literal(resolver, &unquote(&resolved))
}

fn decode_yaml_literal(resolver: &str, input: &str) -> Result<Value> {
    serde_yaml::from_str::<Value>(input).map_err(|e| CrabError::Configuration {
        key: format!("hydra resolver {resolver}"),
        origin: format!("{resolver} could not parse value: {e}"),
    })
}

fn resolve_oc_deprecated(args: &str, root: &Value, path: &[String]) -> Result<Value> {
    let parts = split_resolver_args(args)?;
    if !(1..=2).contains(&parts.len()) {
        return Err(CrabError::Configuration {
            key: "hydra resolver oc.deprecated".to_owned(),
            origin: "oc.deprecated requires one key and optional message".to_owned(),
        });
    }

    let key = resolve_select_key(&parts[0], root, path)?;
    match lookup_config_path(root, &key, path) {
        Some(value) if !is_missing_value(value) => Ok(value.clone()),
        Some(_) => Ok(Value::Null),
        None => Err(CrabError::Configuration {
            key: format!("hydra resolver oc.deprecated: {key}"),
            origin: "replacement key not found".to_owned(),
        }),
    }
}

fn resolve_oc_dict_keys(args: &str, root: &Value, path: &[String]) -> Result<Value> {
    let map = resolve_oc_dict_map("oc.dict.keys", args, root, path)?;
    let keys = map
        .keys()
        .map(mapping_key_to_string)
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .map(Value::String)
        .collect();
    Ok(Value::Sequence(keys))
}

fn resolve_oc_dict_values(args: &str, root: &Value, path: &[String]) -> Result<Value> {
    let map = resolve_oc_dict_map("oc.dict.values", args, root, path)?;
    Ok(Value::Sequence(map.values().cloned().collect()))
}

fn resolve_oc_dict_map<'a>(
    resolver: &str,
    args: &str,
    root: &'a Value,
    path: &[String],
) -> Result<&'a Mapping> {
    let parts = split_resolver_args(args)?;
    if parts.len() != 1 {
        return Err(CrabError::Configuration {
            key: format!("hydra resolver {resolver}"),
            origin: format!("{resolver} requires exactly one config path"),
        });
    }

    let key = resolve_select_key(&parts[0], root, path)?;
    match lookup_config_path(root, &key, path) {
        Some(Value::Mapping(map)) => Ok(map),
        Some(_) => Err(CrabError::Configuration {
            key: format!("hydra resolver {resolver}: {key}"),
            origin: format!("{resolver} requires a mapping config node"),
        }),
        None => Err(CrabError::Configuration {
            key: format!("hydra resolver {resolver}: {key}"),
            origin: "config key not found".to_owned(),
        }),
    }
}

fn resolve_oc_select(args: &str, root: &Value, path: &[String]) -> Result<Value> {
    let parts = split_resolver_args(args)?;
    if !(1..=2).contains(&parts.len()) {
        return Err(CrabError::Configuration {
            key: "hydra resolver oc.select".to_owned(),
            origin: "oc.select requires one key and optional default".to_owned(),
        });
    }

    let key = resolve_select_key(&parts[0], root, path)?;
    if let Some(value) = lookup_config_path(root, &key, path)
        && !is_missing_value(value)
    {
        return Ok(value.clone());
    }

    if parts.len() == 2 {
        return resolve_resolver_value_arg(&parts[1], root, path);
    }

    Ok(Value::Null)
}

fn resolve_select_key(arg: &str, root: &Value, path: &[String]) -> Result<String> {
    let trimmed = arg.trim();
    if is_quoted(trimmed) {
        return Ok(unquote(trimmed));
    }
    resolve_resolver_arg(trimmed, root, path)
}

fn resolve_resolver_value_arg(arg: &str, root: &Value, path: &[String]) -> Result<Value> {
    let trimmed = arg.trim();
    if let Some(expr) = full_interpolation_expr(trimmed) {
        return match resolve_interpolation_expr(expr, root, path)? {
            Some(value) => Ok(value),
            None => Ok(Value::String(unquote(trimmed))),
        };
    }

    let mut resolved = trimmed.to_owned();
    resolve_string_interpolation(&mut resolved, root, path)?;
    let resolved = unquote(&resolved);
    if resolved == "null" {
        return Ok(Value::Null);
    }
    Ok(Value::String(resolved))
}

fn is_missing_value(value: &Value) -> bool {
    matches!(value, Value::String(value) if value == "???")
}

fn resolve_resolver_arg(arg: &str, root: &Value, path: &[String]) -> Result<String> {
    let trimmed = arg.trim();
    if let Some(expr) = full_interpolation_expr(trimmed) {
        return match resolve_interpolation_expr(expr, root, path)? {
            Some(value) => scalar_to_string(&value),
            None => Ok(trimmed.to_owned()),
        };
    }

    let mut resolved = trimmed.to_owned();
    resolve_string_interpolation(&mut resolved, root, path)?;
    Ok(unquote(&resolved))
}

fn full_interpolation_expr(input: &str) -> Option<&str> {
    let inner = input.strip_prefix("${")?;
    let end = find_interpolation_end(inner)?;
    if end + 1 == inner.len() {
        Some(&inner[..end])
    } else {
        None
    }
}

fn split_resolver_args(input: &str) -> Result<Vec<String>> {
    let mut args = Vec::new();
    let mut start = 0usize;
    let mut depth = 0i32;
    let mut interpolation_depth = 0i32;
    let mut quote = None;
    let mut escaped = false;
    let bytes = input.as_bytes();
    let mut i = 0usize;

    while i < input.len() {
        let ch = input[i..]
            .chars()
            .next()
            .ok_or_else(|| CrabError::Internal("invalid resolver argument cursor".to_owned()))?;
        if let Some(quote_ch) = quote {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == quote_ch {
                quote = None;
            }
            i += ch.len_utf8();
            continue;
        }

        if bytes[i] == b'$' && i + 1 < bytes.len() && bytes[i + 1] == b'{' {
            interpolation_depth += 1;
            i += 2;
            continue;
        }

        match ch {
            '\'' | '"' => quote = Some(ch),
            '(' | '[' | '{' => depth += 1,
            '}' if interpolation_depth > 0 => interpolation_depth -= 1,
            ')' | ']' | '}' => {
                depth -= 1;
                if depth < 0 {
                    return Err(CrabError::Configuration {
                        key: format!("unbalanced hydra resolver arguments: {input}"),
                        origin: "hydra".to_owned(),
                    });
                }
            }
            ',' if depth == 0 && interpolation_depth == 0 => {
                push_resolver_arg(input, &mut args, &input[start..i])?;
                start = i + ch.len_utf8();
            }
            _ => {}
        }
        i += ch.len_utf8();
    }

    if quote.is_some() || depth != 0 || interpolation_depth != 0 {
        return Err(CrabError::Configuration {
            key: format!("unbalanced hydra resolver arguments: {input}"),
            origin: "hydra".to_owned(),
        });
    }

    if start < input.len() || !input.trim().is_empty() {
        push_resolver_arg(input, &mut args, &input[start..])?;
    }
    Ok(args)
}

fn push_resolver_arg(input: &str, args: &mut Vec<String>, raw: &str) -> Result<()> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(CrabError::Configuration {
            key: format!("empty hydra resolver argument: {input}"),
            origin: "hydra".to_owned(),
        });
    }
    args.push(trimmed.to_owned());
    Ok(())
}

fn unquote(input: &str) -> String {
    let trimmed = input.trim();
    if is_quoted(trimmed) {
        return trimmed[1..trimmed.len() - 1].to_owned();
    }
    trimmed.to_owned()
}

fn is_quoted(input: &str) -> bool {
    input.len() >= 2
        && ((input.starts_with('"') && input.ends_with('"'))
            || (input.starts_with('\'') && input.ends_with('\'')))
}

fn lookup_config_path<'a>(
    root: &'a Value,
    path: &str,
    current_path: &[String],
) -> Option<&'a Value> {
    if path.starts_with('.') {
        return lookup_relative_path(root, path, current_path);
    }
    lookup_path(root, path)
}

fn lookup_relative_path<'a>(
    root: &'a Value,
    path: &str,
    current_path: &[String],
) -> Option<&'a Value> {
    let dot_count = path.chars().take_while(|ch| *ch == '.').count();
    if dot_count == 0 {
        return lookup_path(root, path);
    }

    let mut segments = current_path.to_vec();
    if !segments.is_empty() {
        segments.pop();
    }
    for _ in 1..dot_count {
        segments.pop()?;
    }

    let tail = &path[dot_count..];
    if !tail.is_empty() {
        segments.extend(
            tail.split('.')
                .filter(|part| !part.is_empty())
                .map(str::to_owned),
        );
    }
    lookup_segments(root, segments.iter().map(String::as_str))
}

fn lookup_path<'a>(root: &'a Value, path: &str) -> Option<&'a Value> {
    lookup_segments(root, path.split('.').filter(|part| !part.is_empty()))
}

fn lookup_segments<I>(root: &Value, segments: I) -> Option<&Value>
where
    I: IntoIterator,
    I::Item: AsRef<str>,
{
    let mut cursor = root;
    for segment in segments {
        let segment = segment.as_ref();
        match cursor {
            Value::Mapping(map) => {
                cursor = map.get(Value::String(segment.to_owned()))?;
            }
            Value::Sequence(items) => {
                let index = segment.parse::<usize>().ok()?;
                cursor = items.get(index)?;
            }
            _ => return None,
        }
    }
    Some(cursor)
}

fn scalar_to_string(value: &Value) -> Result<String> {
    match value {
        Value::String(s) => Ok(s.clone()),
        Value::Bool(b) => Ok(b.to_string()),
        Value::Number(n) => Ok(n.to_string()),
        Value::Null => Ok("null".to_owned()),
        _ => Err(CrabError::Configuration {
            key: "hydra interpolation".to_owned(),
            origin: "only scalar interpolation values are supported".to_owned(),
        }),
    }
}

fn mapping_key_to_string(value: &Value) -> Result<String> {
    match value {
        Value::String(s) => Ok(s.clone()),
        Value::Bool(b) => Ok(b.to_string()),
        Value::Number(n) => Ok(n.to_string()),
        Value::Null => Ok("null".to_owned()),
        _ => Err(CrabError::Configuration {
            key: "hydra resolver oc.dict".to_owned(),
            origin: "mapping keys must be scalar values".to_owned(),
        }),
    }
}

fn mapping_key_to_path_segment(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Bool(b) => Some(b.to_string()),
        Value::Number(n) => Some(n.to_string()),
        Value::Null => Some("null".to_owned()),
        _ => None,
    }
}

fn write_params_yaml(tmpdir: &Path, value: &Value) -> Result<()> {
    let params_path = tmpdir.join(DEFAULT_OUTPUT);
    let mut text = serde_yaml::to_string(value).map_err(|e| CrabError::Configuration {
        key: format!("hydra composed params serialization error: {e}"),
        origin: "hydra".to_owned(),
    })?;
    if !text.ends_with('\n') {
        text.push('\n');
    }
    let mode = fs::metadata(&params_path)
        .map(|metadata| {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                metadata.permissions().mode()
            }
            #[cfg(not(unix))]
            {
                let _ = metadata;
                0o644
            }
        })
        .unwrap_or(0o644);
    write_atomic(&params_path, text.as_bytes(), uuid::Uuid::now_v7(), mode)
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn enable_hydra(root: &Path) {
        fs::create_dir_all(root.join(".crab")).unwrap();
        fs::write(root.join(CONFIG_REL), "[hydra]\nenabled = true\n").unwrap();
    }

    fn write_config_tree(root: &Path) {
        fs::create_dir_all(root.join("conf/dataset")).unwrap();
        fs::create_dir_all(root.join("conf/train/model")).unwrap();
        fs::create_dir_all(root.join("conf/train/optimizer")).unwrap();
        fs::write(
            root.join("conf/config.yaml"),
            "defaults:\n  - dataset: imagenette\n  - train/model: resnet\n  - train/optimizer: sgd\n",
        )
        .unwrap();
        fs::write(
            root.join("conf/dataset/imagenette.yaml"),
            "url: s3://imagenette\noutput_folder: imagenette\n",
        )
        .unwrap();
        fs::write(
            root.join("conf/train/model/resnet.yaml"),
            "name: ResNet\nsize: 50\n",
        )
        .unwrap();
        fs::write(
            root.join("conf/train/model/efficientnet.yaml"),
            "name: EfficientNet\nsize: b0\n",
        )
        .unwrap();
        fs::write(
            root.join("conf/train/optimizer/sgd.yaml"),
            "name: SGD\nlr: 0.001\n",
        )
        .unwrap();
        fs::write(
            root.join("conf/train/optimizer/adam.yaml"),
            "name: Adam\nlr: 0.0001\n",
        )
        .unwrap();
    }

    #[test]
    fn disabled_hydra_returns_original_overrides() {
        let tmp = TempDir::new().unwrap();
        let overrides = BTreeMap::from([("train/model".to_owned(), "resnet".to_owned())]);

        let remaining = compose_if_enabled(tmp.path(), &overrides).unwrap();

        assert_eq!(remaining, overrides);
    }

    #[test]
    fn composes_defaults_into_params_yaml() {
        let tmp = TempDir::new().unwrap();
        enable_hydra(tmp.path());
        write_config_tree(tmp.path());

        let remaining = compose_if_enabled(tmp.path(), &BTreeMap::new()).unwrap();

        assert!(remaining.is_empty());
        let params = fs::read_to_string(tmp.path().join(DEFAULT_OUTPUT)).unwrap();
        assert!(params.contains("dataset:"));
        assert!(params.contains("output_folder: imagenette"));
        assert!(params.contains("model:"));
        assert!(params.contains("name: ResNet"));
        assert!(params.contains("optimizer:"));
        assert!(params.contains("lr: 0.001"));
    }

    #[test]
    fn group_overrides_select_different_config_files() {
        let tmp = TempDir::new().unwrap();
        enable_hydra(tmp.path());
        write_config_tree(tmp.path());
        let overrides = BTreeMap::from([
            ("train/model".to_owned(), "efficientnet".to_owned()),
            ("train.optimizer.lr".to_owned(), "0.01".to_owned()),
        ]);

        let remaining = compose_if_enabled(tmp.path(), &overrides).unwrap();

        assert_eq!(
            remaining,
            BTreeMap::from([("train.optimizer.lr".to_owned(), "0.01".to_owned())])
        );
        let params = fs::read_to_string(tmp.path().join(DEFAULT_OUTPUT)).unwrap();
        assert!(params.contains("name: EfficientNet"));
        assert!(params.contains("size: b0"));
        assert!(params.contains("name: SGD"));
    }

    #[test]
    fn package_directive_global_places_config_at_root() {
        let tmp = TempDir::new().unwrap();
        enable_hydra(tmp.path());
        fs::create_dir_all(tmp.path().join("conf/env")).unwrap();
        fs::write(
            tmp.path().join("conf/config.yaml"),
            "defaults:\n  - env: prod\n",
        )
        .unwrap();
        fs::write(
            tmp.path().join("conf/env/prod.yaml"),
            "# @package _global_\ndb:\n  host: 10.0.0.11\nwebserver:\n  port: 443\n",
        )
        .unwrap();

        compose_if_enabled(tmp.path(), &BTreeMap::new()).unwrap();

        let params = fs::read_to_string(tmp.path().join(DEFAULT_OUTPUT)).unwrap();
        assert!(params.contains("db:\n  host: 10.0.0.11"));
        assert!(params.contains("webserver:\n  port: 443"));
        assert!(!params.contains("env:"));
    }

    #[test]
    fn package_directive_expands_group_and_name_keywords() {
        let tmp = TempDir::new().unwrap();
        enable_hydra(tmp.path());
        fs::create_dir_all(tmp.path().join("conf/db")).unwrap();
        fs::write(
            tmp.path().join("conf/config.yaml"),
            "defaults:\n  - db: mysql\n",
        )
        .unwrap();
        fs::write(
            tmp.path().join("conf/db/mysql.yaml"),
            "# @package _group_._name_\nhost: localhost\nport: 3306\n",
        )
        .unwrap();

        compose_if_enabled(tmp.path(), &BTreeMap::new()).unwrap();

        let params = fs::read_to_string(tmp.path().join(DEFAULT_OUTPUT)).unwrap();
        assert!(params.contains("db:\n  mysql:\n    host: localhost"));
        assert!(params.contains("port: 3306"));
    }

    #[test]
    fn defaults_list_package_override_wins_over_package_directive() {
        let tmp = TempDir::new().unwrap();
        enable_hydra(tmp.path());
        fs::create_dir_all(tmp.path().join("conf/db")).unwrap();
        fs::write(
            tmp.path().join("conf/config.yaml"),
            "defaults:\n  - db@primary: mysql\n",
        )
        .unwrap();
        fs::write(
            tmp.path().join("conf/db/mysql.yaml"),
            "# @package _global_\nhost: localhost\n",
        )
        .unwrap();

        compose_if_enabled(tmp.path(), &BTreeMap::new()).unwrap();

        let params = fs::read_to_string(tmp.path().join(DEFAULT_OUTPUT)).unwrap();
        assert!(params.contains("primary:\n  host: localhost"));
        assert!(!params.starts_with("host: localhost"));
    }

    #[test]
    fn self_order_controls_primary_config_precedence() {
        let tmp = TempDir::new().unwrap();
        enable_hydra(tmp.path());
        fs::create_dir_all(tmp.path().join("conf/db")).unwrap();
        fs::write(
            tmp.path().join("conf/config.yaml"),
            "defaults:\n  - db: mysql\n  - _self_\ndb:\n  user: root\n",
        )
        .unwrap();
        fs::write(
            tmp.path().join("conf/db/mysql.yaml"),
            "user: mysql\nport: 3306\n",
        )
        .unwrap();

        compose_if_enabled(tmp.path(), &BTreeMap::new()).unwrap();

        let params = fs::read_to_string(tmp.path().join(DEFAULT_OUTPUT)).unwrap();
        assert!(params.contains("user: root"));
        assert!(params.contains("port: 3306"));
    }

    #[test]
    fn simple_interpolations_resolve_after_merge() {
        let tmp = TempDir::new().unwrap();
        enable_hydra(tmp.path());
        fs::create_dir_all(tmp.path().join("conf/dataset")).unwrap();
        fs::write(
            tmp.path().join("conf/config.yaml"),
            "defaults:\n  - dataset: local\n",
        )
        .unwrap();
        fs::write(
            tmp.path().join("conf/dataset/local.yaml"),
            "dir: raw\nfile: data.csv\npath: ${dataset.dir}/${dataset.file}\n",
        )
        .unwrap();

        compose_if_enabled(tmp.path(), &BTreeMap::new()).unwrap();

        let params = fs::read_to_string(tmp.path().join(DEFAULT_OUTPUT)).unwrap();
        assert!(params.contains("path: raw/data.csv"));
    }

    #[test]
    fn relative_interpolations_resolve_sibling_parent_and_sequence_values() {
        let tmp = TempDir::new().unwrap();
        enable_hydra(tmp.path());
        fs::create_dir_all(tmp.path().join("conf")).unwrap();
        fs::write(
            tmp.path().join("conf/config.yaml"),
            "client:\n  host: localhost\n  port: 8080\n  url: http://${.host}:${.port}\n  description: Client of ${.url}\nmodel:\n  backbone:\n    out_features: [c4, c5]\n  head:\n    in_features: ${..backbone.out_features}\nfirst_feature: ${model.backbone.out_features.0}\n",
        )
        .unwrap();

        compose_if_enabled(tmp.path(), &BTreeMap::new()).unwrap();

        let params = fs::read_to_string(tmp.path().join(DEFAULT_OUTPUT)).unwrap();
        let value: Value = serde_yaml::from_str(&params).unwrap();
        assert_eq!(
            lookup_path(&value, "client.url").and_then(Value::as_str),
            Some("http://localhost:8080")
        );
        assert_eq!(
            lookup_path(&value, "client.description").and_then(Value::as_str),
            Some("Client of http://localhost:8080")
        );
        assert_eq!(
            lookup_path(&value, "model.head.in_features"),
            lookup_path(&value, "model.backbone.out_features")
        );
        assert_eq!(
            lookup_path(&value, "first_feature").and_then(Value::as_str),
            Some("c4")
        );
    }

    #[test]
    fn join_resolver_resolves_nested_interpolation_args() {
        let tmp = TempDir::new().unwrap();
        enable_hydra(tmp.path());
        fs::create_dir_all(tmp.path().join("conf")).unwrap();
        fs::write(
            tmp.path().join("conf/config.yaml"),
            "dir: raw/data\nrelpath: dataset.csv\nfullpath: ${join:${dir},${relpath}}\n",
        )
        .unwrap();

        compose_if_enabled(tmp.path(), &BTreeMap::new()).unwrap();

        let params = fs::read_to_string(tmp.path().join(DEFAULT_OUTPUT)).unwrap();
        assert!(params.contains("fullpath: raw/data/dataset.csv"));
    }

    #[test]
    fn oc_decode_resolver_parses_inline_scalars_and_maps() {
        let tmp = TempDir::new().unwrap();
        enable_hydra(tmp.path());
        fs::create_dir_all(tmp.path().join("conf")).unwrap();
        fs::write(
            tmp.path().join("conf/config.yaml"),
            "enabled: ${oc.decode:true}\ncount: ${oc.decode:42}\nlabels: \"${oc.decode:{stage: train}}\"\n",
        )
        .unwrap();

        compose_if_enabled(tmp.path(), &BTreeMap::new()).unwrap();

        let params = fs::read_to_string(tmp.path().join(DEFAULT_OUTPUT)).unwrap();
        assert!(params.contains("enabled: true"));
        assert!(params.contains("count: 42"));
        assert!(params.contains("stage: train"));
    }

    #[test]
    fn oc_decode_resolver_decodes_environment_values() {
        let tmp = TempDir::new().unwrap();
        enable_hydra(tmp.path());
        fs::create_dir_all(tmp.path().join("conf")).unwrap();
        fs::write(
            tmp.path().join("conf/config.yaml"),
            "port: ${oc.decode:${oc.env:CRAB_TEST_HYDRA_PORT}}\nnodes: ${oc.decode:${oc.env:CRAB_TEST_HYDRA_NODES}}\n",
        )
        .unwrap();
        // SAFETY: These env var names are unique to this test and are
        // reset immediately after composition.
        unsafe {
            std::env::set_var("CRAB_TEST_HYDRA_PORT", "3308");
            std::env::set_var("CRAB_TEST_HYDRA_NODES", "[host1, host2, host3]");
        }

        compose_if_enabled(tmp.path(), &BTreeMap::new()).unwrap();

        unsafe {
            std::env::remove_var("CRAB_TEST_HYDRA_PORT");
            std::env::remove_var("CRAB_TEST_HYDRA_NODES");
        }
        let params = fs::read_to_string(tmp.path().join(DEFAULT_OUTPUT)).unwrap();
        assert!(params.contains("port: 3308"));
        assert!(params.contains("- host1"));
        assert!(params.contains("- host2"));
        assert!(params.contains("- host3"));
    }

    #[test]
    fn oc_decode_resolver_preserves_null_from_nested_resolver() {
        let tmp = TempDir::new().unwrap();
        enable_hydra(tmp.path());
        fs::create_dir_all(tmp.path().join("conf")).unwrap();
        fs::write(
            tmp.path().join("conf/config.yaml"),
            "timeout: ${oc.decode:${oc.env:CRAB_TEST_HYDRA_TIMEOUT,null}}\n",
        )
        .unwrap();
        // SAFETY: The env var name is unique to this test and is
        // removed before composition, so no other test relies on it.
        unsafe {
            std::env::remove_var("CRAB_TEST_HYDRA_TIMEOUT");
        }

        compose_if_enabled(tmp.path(), &BTreeMap::new()).unwrap();

        let params = fs::read_to_string(tmp.path().join(DEFAULT_OUTPUT)).unwrap();
        assert!(params.contains("timeout: null"));
    }

    #[test]
    fn oc_create_resolver_builds_config_node_from_env_yaml() {
        let tmp = TempDir::new().unwrap();
        enable_hydra(tmp.path());
        fs::create_dir_all(tmp.path().join("conf")).unwrap();
        fs::write(
            tmp.path().join("conf/config.yaml"),
            "created: \"${oc.create:${oc.env:CRAB_TEST_HYDRA_CREATED}}\"\n",
        )
        .unwrap();
        // SAFETY: The env var name is unique to this test and is
        // reset immediately after composition.
        unsafe {
            std::env::set_var("CRAB_TEST_HYDRA_CREATED", "a: 10\nb: [x, y]\n");
        }

        compose_if_enabled(tmp.path(), &BTreeMap::new()).unwrap();

        unsafe {
            std::env::remove_var("CRAB_TEST_HYDRA_CREATED");
        }
        let params = fs::read_to_string(tmp.path().join(DEFAULT_OUTPUT)).unwrap();
        assert!(params.contains("a: 10"));
        assert!(params.contains("- x"));
        assert!(params.contains("- y"));
    }

    #[test]
    fn oc_deprecated_resolver_reads_replacement_key() {
        let tmp = TempDir::new().unwrap();
        enable_hydra(tmp.path());
        fs::create_dir_all(tmp.path().join("conf")).unwrap();
        fs::write(
            tmp.path().join("conf/config.yaml"),
            "new_key: 10\nold_key: \"${oc.deprecated:new_key,'Use new_key'}\"\n",
        )
        .unwrap();

        compose_if_enabled(tmp.path(), &BTreeMap::new()).unwrap();

        let params = fs::read_to_string(tmp.path().join(DEFAULT_OUTPUT)).unwrap();
        assert!(params.contains("new_key: 10"));
        assert!(params.contains("old_key: 10"));
    }

    #[test]
    fn oc_dict_resolvers_return_keys_and_values() {
        let tmp = TempDir::new().unwrap();
        enable_hydra(tmp.path());
        fs::create_dir_all(tmp.path().join("conf")).unwrap();
        fs::write(
            tmp.path().join("conf/config.yaml"),
            "workers:\n  node3: 10.0.0.2\n  node7: 10.0.0.9\nnodes: \"${oc.dict.keys: workers}\"\nips: \"${oc.dict.values: workers}\"\n",
        )
        .unwrap();

        compose_if_enabled(tmp.path(), &BTreeMap::new()).unwrap();

        let params = fs::read_to_string(tmp.path().join(DEFAULT_OUTPUT)).unwrap();
        assert!(params.contains("- node3"));
        assert!(params.contains("- node7"));
        assert!(params.contains("- 10.0.0.2"));
        assert!(params.contains("- 10.0.0.9"));
    }

    #[test]
    fn oc_dict_resolver_rejects_non_mapping_nodes() {
        let tmp = TempDir::new().unwrap();
        enable_hydra(tmp.path());
        fs::create_dir_all(tmp.path().join("conf")).unwrap();
        fs::write(
            tmp.path().join("conf/config.yaml"),
            "workers: [node3, node7]\nnodes: \"${oc.dict.keys: workers}\"\n",
        )
        .unwrap();

        let err = compose_if_enabled(tmp.path(), &BTreeMap::new()).unwrap_err();
        match err {
            CrabError::Configuration { origin, .. } => {
                assert!(origin.contains("requires a mapping config node"));
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn oc_select_resolver_uses_default_when_key_is_missing() {
        let tmp = TempDir::new().unwrap();
        enable_hydra(tmp.path());
        fs::create_dir_all(tmp.path().join("conf")).unwrap();
        fs::write(
            tmp.path().join("conf/config.yaml"),
            "path: ${oc.select:output,/tmp}\n",
        )
        .unwrap();

        compose_if_enabled(tmp.path(), &BTreeMap::new()).unwrap();

        let params = fs::read_to_string(tmp.path().join(DEFAULT_OUTPUT)).unwrap();
        assert!(params.contains("path: /tmp"));
    }

    #[test]
    fn oc_select_resolver_reads_existing_key() {
        let tmp = TempDir::new().unwrap();
        enable_hydra(tmp.path());
        fs::create_dir_all(tmp.path().join("conf")).unwrap();
        fs::write(
            tmp.path().join("conf/config.yaml"),
            "output: /etc/config\npath: ${oc.select:output,/tmp}\n",
        )
        .unwrap();

        compose_if_enabled(tmp.path(), &BTreeMap::new()).unwrap();

        let params = fs::read_to_string(tmp.path().join(DEFAULT_OUTPUT)).unwrap();
        assert!(params.contains("path: /etc/config"));
    }

    #[test]
    fn oc_select_resolver_accepts_relative_key_and_default() {
        let tmp = TempDir::new().unwrap();
        enable_hydra(tmp.path());
        fs::create_dir_all(tmp.path().join("conf")).unwrap();
        fs::write(
            tmp.path().join("conf/config.yaml"),
            "runtime:\n  default: cpu\n  selected: ${oc.select:.device,${.default}}\n",
        )
        .unwrap();

        compose_if_enabled(tmp.path(), &BTreeMap::new()).unwrap();

        let params = fs::read_to_string(tmp.path().join(DEFAULT_OUTPUT)).unwrap();
        assert!(params.contains("selected: cpu"));
    }

    #[test]
    fn oc_select_resolver_accepts_quoted_colon_key() {
        let tmp = TempDir::new().unwrap();
        enable_hydra(tmp.path());
        fs::create_dir_all(tmp.path().join("conf")).unwrap();
        fs::write(
            tmp.path().join("conf/config.yaml"),
            "\"a:b\": 10\ngood: ${oc.select:'a:b'}\n",
        )
        .unwrap();

        compose_if_enabled(tmp.path(), &BTreeMap::new()).unwrap();

        let params = fs::read_to_string(tmp.path().join(DEFAULT_OUTPUT)).unwrap();
        assert!(params.contains("good: 10"));
    }

    #[test]
    fn oc_select_resolver_treats_missing_sentinel_as_null_or_default() {
        let tmp = TempDir::new().unwrap();
        enable_hydra(tmp.path());
        fs::create_dir_all(tmp.path().join("conf")).unwrap();
        fs::write(
            tmp.path().join("conf/config.yaml"),
            "missing: ???\nselect: ${oc.select:missing}\nwith_default: ${oc.select:missing,default value}\n",
        )
        .unwrap();

        compose_if_enabled(tmp.path(), &BTreeMap::new()).unwrap();

        let params = fs::read_to_string(tmp.path().join(DEFAULT_OUTPUT)).unwrap();
        assert!(params.contains("select: null"));
        assert!(params.contains("with_default: default value"));
    }

    #[test]
    fn oc_env_resolver_uses_default_when_missing() {
        let tmp = TempDir::new().unwrap();
        enable_hydra(tmp.path());
        fs::create_dir_all(tmp.path().join("conf")).unwrap();
        fs::write(
            tmp.path().join("conf/config.yaml"),
            "user: ${oc.env:CRAB_TEST_HYDRA_MISSING_USER,crab}\n",
        )
        .unwrap();
        // SAFETY: The env var name is unique to this test and is
        // removed before composition, so no other test relies on it.
        unsafe {
            std::env::remove_var("CRAB_TEST_HYDRA_MISSING_USER");
        }

        compose_if_enabled(tmp.path(), &BTreeMap::new()).unwrap();

        let params = fs::read_to_string(tmp.path().join(DEFAULT_OUTPUT)).unwrap();
        assert!(params.contains("user: crab"));
    }

    #[test]
    fn oc_env_resolver_preserves_null_default() {
        let tmp = TempDir::new().unwrap();
        enable_hydra(tmp.path());
        fs::create_dir_all(tmp.path().join("conf")).unwrap();
        fs::write(
            tmp.path().join("conf/config.yaml"),
            "secret: ${oc.env:CRAB_TEST_HYDRA_MISSING_SECRET,null}\n",
        )
        .unwrap();
        // SAFETY: The env var name is unique to this test and is
        // removed before composition, so no other test relies on it.
        unsafe {
            std::env::remove_var("CRAB_TEST_HYDRA_MISSING_SECRET");
        }

        compose_if_enabled(tmp.path(), &BTreeMap::new()).unwrap();

        let params = fs::read_to_string(tmp.path().join(DEFAULT_OUTPUT)).unwrap();
        assert!(params.contains("secret: null"));
    }

    #[test]
    fn oc_env_resolver_reads_environment_value() {
        let tmp = TempDir::new().unwrap();
        enable_hydra(tmp.path());
        fs::create_dir_all(tmp.path().join("conf")).unwrap();
        fs::write(
            tmp.path().join("conf/config.yaml"),
            "user: ${oc.env:CRAB_TEST_HYDRA_USER,default}\n",
        )
        .unwrap();
        // SAFETY: The env var name is unique to this test and is
        // reset immediately after composition.
        unsafe {
            std::env::set_var("CRAB_TEST_HYDRA_USER", "configured");
        }

        compose_if_enabled(tmp.path(), &BTreeMap::new()).unwrap();

        unsafe {
            std::env::remove_var("CRAB_TEST_HYDRA_USER");
        }
        let params = fs::read_to_string(tmp.path().join(DEFAULT_OUTPUT)).unwrap();
        assert!(params.contains("user: configured"));
    }
}
