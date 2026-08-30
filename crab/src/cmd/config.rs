//! `crab config get <key>` / `crab config set <key> <value>` — read and
//! write configuration entries via the CLI.
//!
//! Keys are routed to the appropriate config file:
//! - Internal keys (checkout.*, push.*, staging.*, etc.) → `.crab/local.toml`
//! - Project keys (remote.url, track.patterns, etc.) → `crab.toml`

use std::io::Write;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::core::config::{CACHE_SERVICE_URL_ENV, REPO_CONFIG_REL};
use crate::core::error::{CrabError, Result};
use crate::core::output::{OutputMode, emit_json};
use crate::core::project_config::CONFIG_FILE_NAME;

/// Project-level configuration keys targeting `crab.toml`.
const PROJECT_KEYS: &[&str] = &[
    "remote.url",
    "track.patterns",
    "hydrate.default",
    "hydrate.auto_patterns",
    "mirror.origin_remote",
    "mirror.crab_remote",
    "auth.provider",
    "auth.storage_provider",
    "workflow.enabled",
    "workflow.discover",
    "workflow.lockfile",
    "workflow.parallelism",
    "workflow.graceful_shutdown_timeout_secs",
    "workflow.max_outs_per_stage",
    "workflow.max_out_bytes",
    "workflow.lock_timeout_secs",
    "workflow.remote_cache_readonly",
];

/// Internal configuration keys targeting `.crab/local.toml`.
const INTERNAL_KEYS: &[&str] = &[
    "checkout.lazy",
    "auth.aws_profile",
    "hydrate.include",
    "hydrate.exclude",
    "hydrate.auto",
    "hydra.enabled",
    "hydra.config_dir",
    "hydra.config_name",
    "push.lock_ttl_secs",
    "push.lock_heartbeat_interval",
    "push.lock_wait_secs",
    "push.max_cas_retries",
    "push.thin_packs",
    "push.upload_concurrency",
    "push.xorb_target_size",
    "fetch.ref_filtering",
    "fetch.object_level_filtering",
    "repack.auto_threshold",
    "staging.segment_target_bytes",
    "staging.segment_target_size",
    "staging.segment_hard_cap_bytes",
    "staging.compact_dead_ratio",
    "staging.compaction_dead_ratio",
    "staging.auto_compact",
    "staging.fd_pool_size",
    "staging.durable_register",
    "staging.retention_hours",
    "cache.service_url",
    "cache.service_mode",
    "cache.push_warming",
    "cache.chunk_cache_dir",
    "cache.service_auth",
    "cache.service_token_path",
    "cache.service_ca_cert",
    "cache.service_client_cert",
    "cache.service_client_key",
    "perf.enabled",
    "perf.shard_bloom",
    "perf.pointer_shard_hint",
    "perf.compress_staging",
    "perf.adaptive_threshold",
    "perf.fastpath_min_size",
    "perf.persist",
    "perf.path",
];

/// Payload emitted by `crab config get --json`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ConfigGetPayload {
    /// The dotted config key that was queried.
    pub key: String,
    /// The resolved value as a string.
    pub value: String,
    /// Where the value came from: `"project"`, `"local"`, `"default"`, or `"env"`.
    pub source: String,
}

struct ResolvedConfigValue {
    value: String,
    source: &'static str,
}

/// Read a config value by dotted key and print it to stdout.
pub fn run_config_get(key: &str, mode: OutputMode) -> Result<()> {
    validate_key(key)?;

    if is_project_key(key) {
        run_project_config_get(key, mode)
    } else {
        let path = PathBuf::from(REPO_CONFIG_REL);
        run_internal_config_get(key, &path, mode)
    }
}

/// Set a config value by dotted key.
pub fn run_config_set(key: &str, value: &str) -> Result<()> {
    validate_key(key)?;

    if is_project_key(key) {
        run_project_config_set(key, value)
    } else {
        let path = PathBuf::from(REPO_CONFIG_REL);
        run_internal_config_set(key, value, &path)
    }
}

/// Set a config value at an explicit config file path.
///
/// Used by `crab clone` to configure lazy checkout in the cloned repo.
pub fn run_config_set_at(key: &str, value: &str, path: &Path) -> Result<()> {
    run_internal_config_set(key, value, path)
}

// ---------------------------------------------------------------------------
// Project config (crab.toml) operations
// ---------------------------------------------------------------------------

/// Read a project config value from `crab.toml`.
fn run_project_config_get(key: &str, mode: OutputMode) -> Result<()> {
    let table = load_or_default()?;
    let value = resolve_dotted_key(&table, key)?;

    if mode == OutputMode::Json {
        let payload = ConfigGetPayload {
            key: key.to_owned(),
            value: value.clone(),
            source: if value.is_empty() {
                "default"
            } else {
                "project"
            }
            .to_owned(),
        };
        emit_json("config.get", "1.0", payload);
        return Ok(());
    }

    if value.is_empty() {
        tracing::debug!(key, "key not set in project config");
    } else {
        println!("{value}");
    }
    Ok(())
}

/// Set a project config value in `crab.toml`.
fn run_project_config_set(key: &str, value: &str) -> Result<()> {
    let mut table = load_or_default()?;
    if table.is_empty() {
        if key != "remote.url" {
            return Err(CrabError::Configuration {
                key: key.to_owned(),
                origin: "crab.toml does not exist; run `crab configure <REMOTE>` first".to_owned(),
            });
        }
        table.insert("version".to_owned(), toml::Value::Integer(1));
    }
    set_dotted_key(&mut table, key, value)?;
    atomic_write(&table)?;
    tracing::info!(key, value, "project config updated");
    Ok(())
}

/// Load `crab.toml` as a TOML table, creating a default if missing.
fn load_or_default() -> Result<toml::Table> {
    let path = find_project_config_path();
    read_toml(&path)
}

/// Resolve a dotted key (e.g. `"remote.url"`) from a TOML table.
///
/// Returns the value as a string, or an empty string if not set.
fn resolve_dotted_key(table: &toml::Table, key: &str) -> Result<String> {
    let (section, field) = split_key(key)?;
    let value = table.get(section).and_then(|s| s.get(field));

    Ok(match value {
        Some(toml::Value::String(s)) => s.clone(),
        Some(toml::Value::Boolean(b)) => b.to_string(),
        Some(toml::Value::Integer(n)) => n.to_string(),
        Some(toml::Value::Float(f)) => f.to_string(),
        Some(toml::Value::Array(arr)) => {
            let items: Vec<String> = arr
                .iter()
                .map(|item| match item {
                    toml::Value::String(s) => s.clone(),
                    other => other.to_string(),
                })
                .collect();
            items.join(", ")
        }
        Some(other) => other.to_string(),
        None => String::new(),
    })
}

/// Set a dotted key in a TOML table.
///
/// For array-typed keys (`track.patterns`, `hydrate.auto_patterns`),
/// the value is appended. For scalar keys, the value replaces any
/// existing entry.
fn set_dotted_key(table: &mut toml::Table, key: &str, value: &str) -> Result<()> {
    let (section, field) = split_key(key)?;

    // Ensure the section table exists.
    if !table.contains_key(section) {
        table.insert(
            section.to_owned(),
            toml::Value::Table(toml::map::Map::new()),
        );
    }
    let section_table = table
        .get_mut(section)
        .and_then(|v| v.as_table_mut())
        .ok_or_else(|| CrabError::Configuration {
            key: format!("{section} is not a table"),
            origin: CONFIG_FILE_NAME.into(),
        })?;

    match key {
        "track.patterns" | "hydrate.auto_patterns" => {
            // Array keys: append the value.
            let arr = section_table
                .entry(field)
                .or_insert_with(|| toml::Value::Array(Vec::new()));
            let arr = arr.as_array_mut().ok_or_else(|| CrabError::Configuration {
                key: format!("{key} is not an array"),
                origin: CONFIG_FILE_NAME.into(),
            })?;
            arr.push(toml::Value::String(value.to_owned()));
        }
        "hydrate.default" => {
            // Enum key: validate and store as string.
            match value.to_lowercase().as_str() {
                "lazy" | "eager" => {
                    section_table
                        .insert(field.to_owned(), toml::Value::String(value.to_lowercase()));
                }
                _ => {
                    return Err(CrabError::Configuration {
                        key: format!("{key}: expected \"lazy\" or \"eager\", got \"{value}\""),
                        origin: CONFIG_FILE_NAME.into(),
                    });
                }
            }
        }
        "auth.storage_provider" => {
            let provider = crate::cmd::init::parse_storage_provider_arg(value)?;
            section_table.insert(
                field.to_owned(),
                toml::Value::String(provider.toml_value().to_owned()),
            );
        }
        "workflow.enabled" | "workflow.remote_cache_readonly" => {
            let b = parse_bool(value, key, Path::new(CONFIG_FILE_NAME))?;
            section_table.insert(field.to_owned(), toml::Value::Boolean(b));
        }
        "workflow.parallelism"
        | "workflow.graceful_shutdown_timeout_secs"
        | "workflow.max_outs_per_stage"
        | "workflow.max_out_bytes"
        | "workflow.lock_timeout_secs" => {
            let n: i64 = value.parse().map_err(|_| CrabError::Configuration {
                key: format!("{key}: expected integer, got \"{value}\""),
                origin: CONFIG_FILE_NAME.into(),
            })?;
            section_table.insert(field.to_owned(), toml::Value::Integer(n));
        }
        "workflow.discover" => match value.to_lowercase().as_str() {
            "root" | "recursive" => {
                section_table.insert(field.to_owned(), toml::Value::String(value.to_lowercase()));
            }
            _ => {
                return Err(CrabError::Configuration {
                    key: format!("{key}: expected \"root\" or \"recursive\", got \"{value}\""),
                    origin: CONFIG_FILE_NAME.into(),
                });
            }
        },
        "workflow.lockfile" => match value.to_lowercase().as_str() {
            "single" | "split" => {
                section_table.insert(field.to_owned(), toml::Value::String(value.to_lowercase()));
            }
            _ => {
                return Err(CrabError::Configuration {
                    key: format!("{key}: expected \"single\" or \"split\", got \"{value}\""),
                    origin: CONFIG_FILE_NAME.into(),
                });
            }
        },
        _ => {
            // All other project keys are plain strings.
            section_table.insert(field.to_owned(), toml::Value::String(value.to_owned()));
        }
    }

    Ok(())
}

/// Atomic write: serialize to tempfile in same directory, then rename.
fn atomic_write(table: &toml::Table) -> Result<()> {
    let path = find_project_config_path();
    let dir = path.parent().unwrap_or(Path::new("."));

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let body = toml::to_string_pretty(table).map_err(|e| CrabError::Configuration {
        key: format!("failed to serialize crab.toml: {e}"),
        origin: path.display().to_string(),
    })?;

    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(format!("# Crab project configuration\n\n{body}").as_bytes())?;
    tmp.persist(&path).map_err(|e| e.error)?;
    Ok(())
}

/// Discover the `crab.toml` path. Walks up from CWD looking for an
/// existing file; falls back to CWD if not found.
fn find_project_config_path() -> PathBuf {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut current = cwd.as_path();
    loop {
        let candidate = current.join(CONFIG_FILE_NAME);
        if candidate.is_file() {
            return candidate;
        }
        match current.parent() {
            Some(parent) => current = parent,
            None => break,
        }
    }
    // Not found — default to CWD.
    cwd.join(CONFIG_FILE_NAME)
}

// ---------------------------------------------------------------------------
// Internal config (.crab/local.toml) operations
// ---------------------------------------------------------------------------

/// Testable inner implementation for `get` that accepts an explicit path.
fn run_internal_config_get(key: &str, path: &Path, mode: OutputMode) -> Result<()> {
    let resolved = resolve_internal_config_value(key, path)?;

    if mode == OutputMode::Json {
        let payload = ConfigGetPayload {
            key: key.to_owned(),
            value: resolved.value,
            source: resolved.source.to_owned(),
        };
        emit_json("config.get", "1.0", payload);
        return Ok(());
    }

    if resolved.source == "env" {
        println!("{}", resolved.value);
        return Ok(());
    }

    let table = read_toml(path)?;
    let (section, field) = split_key(key)?;
    let value = table.get(section).and_then(|s| s.get(field));

    match value {
        Some(toml::Value::Array(arr)) => {
            for item in arr {
                if let toml::Value::String(s) = item {
                    println!("{s}");
                } else {
                    println!("{item}");
                }
            }
        }
        Some(toml::Value::Boolean(b)) => println!("{b}"),
        Some(toml::Value::String(s)) => println!("{s}"),
        Some(other) => println!("{other}"),
        None => {
            tracing::debug!(key, "key not set in config");
        }
    }

    Ok(())
}

fn resolve_internal_config_value(key: &str, path: &Path) -> Result<ResolvedConfigValue> {
    if key == "cache.service_url"
        && let Ok(url) = std::env::var(CACHE_SERVICE_URL_ENV)
    {
        let trimmed = url.trim();
        if !trimmed.is_empty() {
            return Ok(ResolvedConfigValue {
                value: trimmed.to_owned(),
                source: "env",
            });
        }
    }

    let table = read_toml(path)?;
    let (section, field) = split_key(key)?;
    let value = table.get(section).and_then(|s| s.get(field));

    Ok(match value {
        Some(toml::Value::Array(arr)) => {
            let items: Vec<String> = arr
                .iter()
                .map(|item| match item {
                    toml::Value::String(s) => s.clone(),
                    other => other.to_string(),
                })
                .collect();
            ResolvedConfigValue {
                value: items.join("\n"),
                source: "local",
            }
        }
        Some(toml::Value::Boolean(b)) => ResolvedConfigValue {
            value: b.to_string(),
            source: "local",
        },
        Some(toml::Value::String(s)) => ResolvedConfigValue {
            value: s.clone(),
            source: "local",
        },
        Some(other) => ResolvedConfigValue {
            value: other.to_string(),
            source: "local",
        },
        None => ResolvedConfigValue {
            value: String::new(),
            source: "default",
        },
    })
}

/// Testable inner implementation for `set` that accepts an explicit path.
fn run_internal_config_set(key: &str, value: &str, path: &Path) -> Result<()> {
    validate_key(key)?;

    let mut table = read_toml(path)?;

    let (section, field) = split_key(key)?;

    // Ensure the section table exists.
    if !table.contains_key(section) {
        table.insert(
            section.to_owned(),
            toml::Value::Table(toml::map::Map::new()),
        );
    }
    let section_table = table
        .get_mut(section)
        .and_then(|v| v.as_table_mut())
        .ok_or_else(|| CrabError::Configuration {
            key: format!("{section} is not a table"),
            origin: path.display().to_string(),
        })?;

    match key {
        "hydrate.include" | "hydrate.exclude" => {
            let arr = section_table
                .entry(field)
                .or_insert_with(|| toml::Value::Array(Vec::new()));
            let arr = arr.as_array_mut().ok_or_else(|| CrabError::Configuration {
                key: format!("{key} is not an array"),
                origin: path.display().to_string(),
            })?;
            arr.push(toml::Value::String(value.to_owned()));
        }
        "checkout.lazy"
        | "hydrate.auto"
        | "hydra.enabled"
        | "push.thin_packs"
        | "fetch.ref_filtering"
        | "fetch.object_level_filtering"
        | "staging.auto_compact"
        | "staging.durable_register"
        | "cache.push_warming"
        | "perf.enabled"
        | "perf.shard_bloom"
        | "perf.pointer_shard_hint"
        | "perf.compress_staging"
        | "perf.adaptive_threshold"
        | "perf.persist" => {
            let b = parse_bool(value, key, path)?;
            section_table.insert(field.to_owned(), toml::Value::Boolean(b));
        }
        "push.lock_ttl_secs"
        | "push.lock_heartbeat_interval"
        | "push.lock_wait_secs"
        | "push.max_cas_retries"
        | "push.upload_concurrency"
        | "push.xorb_target_size"
        | "repack.auto_threshold"
        | "staging.segment_target_bytes"
        | "staging.segment_target_size"
        | "staging.segment_hard_cap_bytes"
        | "staging.fd_pool_size"
        | "staging.retention_hours"
        | "perf.fastpath_min_size" => {
            let n: i64 = value.parse().map_err(|_| CrabError::Configuration {
                key: format!("{key}: expected integer, got \"{value}\""),
                origin: path.display().to_string(),
            })?;
            section_table.insert(field.to_owned(), toml::Value::Integer(n));
        }
        "cache.service_mode" => match value.to_lowercase().as_str() {
            "cache" | "dedup" | "cache+dedup" => {
                section_table.insert(field.to_owned(), toml::Value::String(value.to_lowercase()));
            }
            _ => {
                return Err(CrabError::Configuration {
                    key: format!(
                        "{key}: expected \"cache\", \"dedup\", or \"cache+dedup\", got \"{value}\""
                    ),
                    origin: path.display().to_string(),
                });
            }
        },
        "cache.service_auth" => match value.to_lowercase().as_str() {
            "none" | "psk" | "bearer" | "mtls" => {
                section_table.insert(field.to_owned(), toml::Value::String(value.to_lowercase()));
            }
            _ => {
                return Err(CrabError::Configuration {
                    key: format!(
                        "{key}: expected \"none\", \"psk\", \"bearer\", or \"mtls\", got \"{value}\""
                    ),
                    origin: path.display().to_string(),
                });
            }
        },
        "staging.compact_dead_ratio" | "staging.compaction_dead_ratio" => {
            let f: f64 = value.parse().map_err(|_| CrabError::Configuration {
                key: format!("{key}: expected number, got \"{value}\""),
                origin: path.display().to_string(),
            })?;
            section_table.insert(field.to_owned(), toml::Value::Float(f));
        }
        "remote.url"
        | "auth.aws_profile"
        | "cache.service_url"
        | "cache.chunk_cache_dir"
        | "cache.service_token_path"
        | "cache.service_ca_cert"
        | "cache.service_client_cert"
        | "cache.service_client_key"
        | "perf.path"
        | "hydra.config_dir"
        | "hydra.config_name" => {
            section_table.insert(field.to_owned(), toml::Value::String(value.to_owned()));
        }
        _ => unreachable!("validate_key should have rejected unknown keys"),
    }

    write_toml(path, &table)?;
    tracing::info!(key, value, "config updated");
    Ok(())
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Returns true if the key targets `crab.toml` (project config).
fn is_project_key(key: &str) -> bool {
    PROJECT_KEYS.contains(&key)
}

/// Validate that `key` is one of the supported dotted keys.
fn validate_key(key: &str) -> Result<()> {
    if PROJECT_KEYS.contains(&key) || INTERNAL_KEYS.contains(&key) {
        Ok(())
    } else {
        let all_keys: Vec<&str> = PROJECT_KEYS
            .iter()
            .chain(INTERNAL_KEYS.iter())
            .copied()
            .collect();
        Err(CrabError::InvalidConfigKey {
            key: key.to_owned(),
            valid_keys: all_keys.join(", "),
        })
    }
}

/// Split a dotted key like `"checkout.lazy"` into `("checkout", "lazy")`.
fn split_key(key: &str) -> Result<(&str, &str)> {
    key.split_once('.').ok_or_else(|| CrabError::Configuration {
        key: format!("expected dotted key (section.field), got \"{key}\""),
        origin: "CLI".into(),
    })
}

/// Parse a string as a boolean, accepting `true`/`false` (case-insensitive).
fn parse_bool(value: &str, key: &str, path: &Path) -> Result<bool> {
    match value.to_lowercase().as_str() {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(CrabError::Configuration {
            key: format!("{key}: expected \"true\" or \"false\", got \"{value}\""),
            origin: path.display().to_string(),
        }),
    }
}

/// Read the TOML file into a `toml::Table`, returning an empty table if
/// the file does not exist.
fn read_toml(path: &Path) -> Result<toml::Table> {
    match std::fs::read_to_string(path) {
        Ok(content) => content
            .parse::<toml::Table>()
            .map_err(|e| CrabError::Configuration {
                key: format!("failed to parse TOML: {e}"),
                origin: path.display().to_string(),
            }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(toml::Table::new()),
        Err(e) => Err(e.into()),
    }
}

/// Write a `toml::Table` back to disk atomically (tempfile + rename).
fn write_toml(path: &Path, table: &toml::Table) -> Result<()> {
    let content = toml::to_string_pretty(table).map_err(|e| CrabError::Configuration {
        key: format!("failed to serialize TOML: {e}"),
        origin: path.display().to_string(),
    })?;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let dir = path.parent().unwrap_or(Path::new("."));
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(content.as_bytes())?;
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn internal_config_path(dir: &Path) -> PathBuf {
        dir.join(".crab").join("local.toml")
    }

    fn setup_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".crab")).unwrap();
        dir
    }

    #[test]
    fn set_and_get_bool_key() {
        let dir = setup_dir();
        let path = internal_config_path(dir.path());

        run_internal_config_set("checkout.lazy", "true", &path).unwrap();

        let table = read_toml(&path).unwrap();
        assert_eq!(table["checkout"]["lazy"], toml::Value::Boolean(true));
    }

    #[test]
    fn set_bool_false() {
        let dir = setup_dir();
        let path = internal_config_path(dir.path());

        run_internal_config_set("hydrate.auto", "false", &path).unwrap();

        let table = read_toml(&path).unwrap();
        assert_eq!(table["hydrate"]["auto"], toml::Value::Boolean(false));
    }

    #[test]
    fn set_aws_profile_is_local() {
        let dir = setup_dir();
        let path = internal_config_path(dir.path());

        run_internal_config_set("auth.aws_profile", "ml-team", &path).unwrap();

        let table = read_toml(&path).unwrap();
        assert_eq!(
            table["auth"]["aws_profile"],
            toml::Value::String("ml-team".to_owned())
        );
    }

    #[test]
    fn set_push_max_cas_retries() {
        let dir = setup_dir();
        let path = internal_config_path(dir.path());

        run_internal_config_set("push.max_cas_retries", "128", &path).unwrap();

        let table = read_toml(&path).unwrap();
        assert_eq!(table["push"]["max_cas_retries"], toml::Value::Integer(128));
    }

    #[test]
    fn set_hydra_config_keys() {
        let dir = setup_dir();
        let path = internal_config_path(dir.path());

        run_internal_config_set("hydra.enabled", "true", &path).unwrap();
        run_internal_config_set("hydra.config_dir", "settings", &path).unwrap();
        run_internal_config_set("hydra.config_name", "experiment.yaml", &path).unwrap();

        let table = read_toml(&path).unwrap();
        assert_eq!(table["hydra"]["enabled"], toml::Value::Boolean(true));
        assert_eq!(
            table["hydra"]["config_dir"],
            toml::Value::String("settings".into())
        );
        assert_eq!(
            table["hydra"]["config_name"],
            toml::Value::String("experiment.yaml".into())
        );
    }

    #[test]
    fn set_workflow_config_keys() {
        let mut table = toml::Table::new();
        set_dotted_key(&mut table, "workflow.enabled", "true").unwrap();
        set_dotted_key(&mut table, "workflow.discover", "recursive").unwrap();
        set_dotted_key(&mut table, "workflow.lockfile", "split").unwrap();
        set_dotted_key(&mut table, "workflow.parallelism", "4").unwrap();
        set_dotted_key(&mut table, "workflow.graceful_shutdown_timeout_secs", "9").unwrap();
        set_dotted_key(&mut table, "workflow.max_outs_per_stage", "16").unwrap();
        set_dotted_key(&mut table, "workflow.max_out_bytes", "1048576").unwrap();
        set_dotted_key(&mut table, "workflow.lock_timeout_secs", "30").unwrap();
        set_dotted_key(&mut table, "workflow.remote_cache_readonly", "true").unwrap();
        assert_eq!(table["workflow"]["enabled"], toml::Value::Boolean(true));
        assert_eq!(
            table["workflow"]["discover"],
            toml::Value::String("recursive".into())
        );
        assert_eq!(
            table["workflow"]["lockfile"],
            toml::Value::String("split".into())
        );
        assert_eq!(table["workflow"]["parallelism"], toml::Value::Integer(4));
        assert_eq!(
            table["workflow"]["graceful_shutdown_timeout_secs"],
            toml::Value::Integer(9)
        );
        assert_eq!(
            table["workflow"]["max_outs_per_stage"],
            toml::Value::Integer(16)
        );
        assert_eq!(
            table["workflow"]["max_out_bytes"],
            toml::Value::Integer(1048576)
        );
        assert_eq!(
            table["workflow"]["lock_timeout_secs"],
            toml::Value::Integer(30)
        );
        assert_eq!(
            table["workflow"]["remote_cache_readonly"],
            toml::Value::Boolean(true)
        );
    }

    #[test]
    fn workflow_enum_values_validate() {
        let mut table = toml::Table::new();

        let err = set_dotted_key(&mut table, "workflow.discover", "everywhere").unwrap_err();
        assert!(matches!(err, CrabError::Configuration { .. }));

        let err = set_dotted_key(&mut table, "workflow.lockfile", "many").unwrap_err();
        assert!(matches!(err, CrabError::Configuration { .. }));
    }

    #[test]
    fn set_cache_service_config_keys() {
        let dir = setup_dir();
        let path = internal_config_path(dir.path());

        run_internal_config_set("cache.service_url", "https://cache.internal:8443", &path).unwrap();
        run_internal_config_set("cache.service_mode", "CACHE+DEDUP", &path).unwrap();
        run_internal_config_set("cache.push_warming", "true", &path).unwrap();
        run_internal_config_set("cache.chunk_cache_dir", "/var/cache/crab/chunks", &path).unwrap();
        run_internal_config_set("cache.service_auth", "PSK", &path).unwrap();
        run_internal_config_set("cache.service_auth", "mTLS", &path).unwrap();
        run_internal_config_set(
            "cache.service_token_path",
            "/run/secrets/cache-token",
            &path,
        )
        .unwrap();
        run_internal_config_set("cache.service_ca_cert", "/etc/crab/cache-ca.pem", &path).unwrap();
        run_internal_config_set(
            "cache.service_client_cert",
            "/etc/crab/cache-client.pem",
            &path,
        )
        .unwrap();
        run_internal_config_set(
            "cache.service_client_key",
            "/etc/crab/cache-client-key.pem",
            &path,
        )
        .unwrap();

        let table = read_toml(&path).unwrap();
        assert_eq!(
            table["cache"]["service_url"],
            toml::Value::String("https://cache.internal:8443".into())
        );
        assert_eq!(
            table["cache"]["service_mode"],
            toml::Value::String("cache+dedup".into())
        );
        assert_eq!(table["cache"]["push_warming"], toml::Value::Boolean(true));
        assert_eq!(
            table["cache"]["chunk_cache_dir"],
            toml::Value::String("/var/cache/crab/chunks".into())
        );
        assert_eq!(
            table["cache"]["service_auth"],
            toml::Value::String("mtls".into())
        );
        assert_eq!(
            table["cache"]["service_token_path"],
            toml::Value::String("/run/secrets/cache-token".into())
        );
        assert_eq!(
            table["cache"]["service_ca_cert"],
            toml::Value::String("/etc/crab/cache-ca.pem".into())
        );
        assert_eq!(
            table["cache"]["service_client_cert"],
            toml::Value::String("/etc/crab/cache-client.pem".into())
        );
        assert_eq!(
            table["cache"]["service_client_key"],
            toml::Value::String("/etc/crab/cache-client-key.pem".into())
        );
    }

    #[test]
    fn cache_service_enums_validate() {
        let dir = setup_dir();
        let path = internal_config_path(dir.path());

        let err = run_internal_config_set("cache.service_mode", "both", &path).unwrap_err();
        assert!(matches!(err, CrabError::Configuration { .. }));

        let err = run_internal_config_set("cache.service_auth", "oauth", &path).unwrap_err();
        assert!(matches!(err, CrabError::Configuration { .. }));
    }

    #[test]
    fn set_array_appends() {
        let dir = setup_dir();
        let path = internal_config_path(dir.path());

        run_internal_config_set("hydrate.include", "*.safetensors", &path).unwrap();
        run_internal_config_set("hydrate.include", "data/**", &path).unwrap();

        let table = read_toml(&path).unwrap();
        let arr = table["hydrate"]["include"].as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0].as_str().unwrap(), "*.safetensors");
        assert_eq!(arr[1].as_str().unwrap(), "data/**");
    }

    #[test]
    fn set_exclude_appends() {
        let dir = setup_dir();
        let path = internal_config_path(dir.path());

        run_internal_config_set("hydrate.exclude", "archive/**", &path).unwrap();

        let table = read_toml(&path).unwrap();
        let arr = table["hydrate"]["exclude"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0].as_str().unwrap(), "archive/**");
    }

    #[test]
    fn get_missing_key_prints_nothing() {
        let dir = setup_dir();
        let path = internal_config_path(dir.path());

        run_internal_config_get("checkout.lazy", &path, OutputMode::Text).unwrap();
    }

    #[test]
    fn get_bool_key() {
        let dir = setup_dir();
        let path = internal_config_path(dir.path());

        run_internal_config_set("checkout.lazy", "true", &path).unwrap();
        run_internal_config_get("checkout.lazy", &path, OutputMode::Text).unwrap();
    }

    #[test]
    fn get_array_key() {
        let dir = setup_dir();
        let path = internal_config_path(dir.path());

        run_internal_config_set("hydrate.include", "*.bin", &path).unwrap();
        run_internal_config_get("hydrate.include", &path, OutputMode::Text).unwrap();
    }

    #[test]
    fn invalid_key_rejected() {
        let err = validate_key("bogus.key").unwrap_err();
        assert!(
            matches!(err, CrabError::InvalidConfigKey { .. }),
            "expected InvalidConfigKey error, got: {err}",
        );
    }

    #[test]
    fn invalid_key_error_lists_valid_keys() {
        let err = validate_key("bogus.key").unwrap_err();
        let msg = err.to_string();
        // Should contain at least one valid key.
        assert!(
            msg.contains("remote.url"),
            "error should list valid keys: {msg}"
        );
        assert!(
            msg.contains("checkout.lazy"),
            "error should list valid keys: {msg}"
        );
    }

    #[test]
    fn invalid_bool_value_rejected() {
        let dir = setup_dir();
        let path = internal_config_path(dir.path());

        let err = run_internal_config_set("checkout.lazy", "yes", &path).unwrap_err();
        assert!(
            matches!(err, CrabError::Configuration { .. }),
            "expected Configuration error, got: {err}",
        );
    }

    #[test]
    fn set_creates_parent_directory() {
        let dir = tempfile::tempdir().unwrap();
        let path = internal_config_path(dir.path());

        run_internal_config_set("checkout.lazy", "true", &path).unwrap();

        assert!(path.exists(), "config file should have been created");
        let table = read_toml(&path).unwrap();
        assert_eq!(table["checkout"]["lazy"], toml::Value::Boolean(true));
    }

    #[test]
    fn set_preserves_existing_keys() {
        let dir = setup_dir();
        let path = internal_config_path(dir.path());

        run_internal_config_set("checkout.lazy", "true", &path).unwrap();
        run_internal_config_set("hydrate.auto", "true", &path).unwrap();

        let table = read_toml(&path).unwrap();
        assert_eq!(table["checkout"]["lazy"], toml::Value::Boolean(true));
        assert_eq!(table["hydrate"]["auto"], toml::Value::Boolean(true));
    }

    #[test]
    fn set_bool_overwrites_previous_value() {
        let dir = setup_dir();
        let path = internal_config_path(dir.path());

        run_internal_config_set("checkout.lazy", "true", &path).unwrap();
        run_internal_config_set("checkout.lazy", "false", &path).unwrap();

        let table = read_toml(&path).unwrap();
        assert_eq!(table["checkout"]["lazy"], toml::Value::Boolean(false));
    }

    #[test]
    fn get_nonexistent_file_prints_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nonexistent/config.toml");

        run_internal_config_get("checkout.lazy", &path, OutputMode::Text).unwrap();
    }

    #[test]
    fn bool_parsing_case_insensitive() {
        let dir = setup_dir();
        let path = internal_config_path(dir.path());

        run_internal_config_set("checkout.lazy", "True", &path).unwrap();
        let table = read_toml(&path).unwrap();
        assert_eq!(table["checkout"]["lazy"], toml::Value::Boolean(true));

        run_internal_config_set("checkout.lazy", "FALSE", &path).unwrap();
        let table = read_toml(&path).unwrap();
        assert_eq!(table["checkout"]["lazy"], toml::Value::Boolean(false));
    }

    // --- Project config tests ---

    #[test]
    fn project_config_set_and_get_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CONFIG_FILE_NAME);

        // Manually set and read to test the core logic.
        let mut table = toml::Table::new();
        set_dotted_key(&mut table, "remote.url", "crab://bucket/repo").unwrap();

        // Write atomically.
        let body = toml::to_string_pretty(&table).unwrap();
        std::fs::write(&path, format!("# Crab project configuration\n\n{body}")).unwrap();

        // Read back.
        let loaded = read_toml(&path).unwrap();
        let value = resolve_dotted_key(&loaded, "remote.url").unwrap();
        assert_eq!(value, "crab://bucket/repo");
    }

    #[test]
    fn project_config_track_patterns_appends() {
        let mut table = toml::Table::new();
        set_dotted_key(&mut table, "track.patterns", "*.bin").unwrap();
        set_dotted_key(&mut table, "track.patterns", "*.safetensors").unwrap();

        let value = resolve_dotted_key(&table, "track.patterns").unwrap();
        assert!(value.contains("*.bin"));
        assert!(value.contains("*.safetensors"));
    }

    #[test]
    fn project_config_hydrate_default_validates() {
        let mut table = toml::Table::new();
        set_dotted_key(&mut table, "hydrate.default", "lazy").unwrap();
        let value = resolve_dotted_key(&table, "hydrate.default").unwrap();
        assert_eq!(value, "lazy");

        set_dotted_key(&mut table, "hydrate.default", "eager").unwrap();
        let value = resolve_dotted_key(&table, "hydrate.default").unwrap();
        assert_eq!(value, "eager");

        let err = set_dotted_key(&mut table, "hydrate.default", "invalid").unwrap_err();
        assert!(matches!(err, CrabError::Configuration { .. }));
    }

    #[test]
    fn project_config_storage_provider_validates() {
        let mut table = toml::Table::new();
        set_dotted_key(&mut table, "auth.storage_provider", "gcs").unwrap();
        let value = resolve_dotted_key(&table, "auth.storage_provider").unwrap();
        assert_eq!(value, "gcs");

        let err = set_dotted_key(&mut table, "auth.storage_provider", "dropbox").unwrap_err();
        assert!(matches!(err, CrabError::Configuration { .. }));
    }

    #[test]
    fn project_key_detection() {
        assert!(is_project_key("remote.url"));
        assert!(is_project_key("track.patterns"));
        assert!(is_project_key("auth.provider"));
        assert!(is_project_key("auth.storage_provider"));
        assert!(!is_project_key("checkout.lazy"));
        assert!(!is_project_key("push.thin_packs"));
    }

    #[test]
    fn validate_key_accepts_all_known_keys() {
        for key in PROJECT_KEYS.iter().chain(INTERNAL_KEYS.iter()) {
            validate_key(key).unwrap_or_else(|_| panic!("key {key} should be valid"));
        }
    }

    #[test]
    fn resolve_dotted_key_returns_empty_for_missing() {
        let table = toml::Table::new();
        let value = resolve_dotted_key(&table, "remote.url").unwrap();
        assert_eq!(value, "");
    }
}
