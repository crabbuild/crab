//! Build script for `crab`.
//!
//! Exposes four `env!`-accessible strings to the crate:
//!
//! - `CRAB_BUILD_VERSION`  — forwarded `CARGO_PKG_VERSION`.
//! - `CRAB_BUILD_GIT_SHA`  — short git sha of the workspace, or `"unknown"`.
//! - `CRAB_BUILD_TIMESTAMP` — human-readable UTC build time.
//! - `CRAB_BUILD_TARGET` — the exact Rust target triple.
//!
//! Also generates `$OUT_DIR/pricing_embedded.rs` from the YAML pricing
//! seed file at `pricing/data/<version>.yaml`. The build fails if the
//! YAML is malformed or any `(provider, region, class)` tuple is missing
//! a required field.
//!
//! All lookups are best-effort: a missing git binary, a non-repo checkout, or
//! a broken system clock falls back to `"unknown"` / `"0"` rather than failing
//! the build.

use serde::Deserialize;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

/// The pricing YAML version to embed. Update when a new seed file is added.
const PRICING_VERSION: &str = "2026-03-01";

/// Fields required on every `(provider, region, class)` tuple.
const REQUIRED_FIELDS: &[&str] = &[
    "gb_month_usd",
    "put_per_k_ops_usd",
    "get_per_k_ops_usd",
    "list_per_k_ops_usd",
    "head_per_k_ops_usd",
    "retrieval_per_gb_usd",
    "min_retention_days",
    "min_object_size_bytes",
    "egress_per_gb_usd",
];

// --- YAML schema types for serde deserialization ---

#[derive(Deserialize)]
struct PricingFile {
    version: String,
    providers: BTreeMap<String, ProviderEntry>,
}

#[derive(Deserialize)]
struct ProviderEntry {
    regions: BTreeMap<String, RegionEntry>,
}

#[derive(Deserialize)]
struct RegionEntry {
    classes: BTreeMap<String, ClassEntry>,
}

/// Each field is stored as a YAML value — strings for decimals, integers
/// for retention/size. We deserialize into `serde_yaml::Value` so we can
/// validate field presence generically, then extract typed values.
#[derive(Deserialize)]
struct ClassEntry {
    gb_month_usd: serde_yaml::Value,
    put_per_k_ops_usd: serde_yaml::Value,
    get_per_k_ops_usd: serde_yaml::Value,
    list_per_k_ops_usd: serde_yaml::Value,
    head_per_k_ops_usd: serde_yaml::Value,
    retrieval_per_gb_usd: serde_yaml::Value,
    min_retention_days: serde_yaml::Value,
    min_object_size_bytes: serde_yaml::Value,
    egress_per_gb_usd: serde_yaml::Value,
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-changed=../.git/HEAD");
    println!("cargo:rerun-if-changed=../.git/index");
    println!("cargo:rerun-if-changed=pricing/data/{PRICING_VERSION}.yaml");
    println!("cargo:rerun-if-env-changed=CRAB_BUILD_VERSION");
    println!("cargo:rerun-if-env-changed=GIT_SHA");

    emit_build_metadata();
    generate_pricing_embedded();
}

// ── Build metadata ──────────────────────────────────────────────────

fn emit_build_metadata() {
    let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown".into());
    println!("cargo:rustc-env=CRAB_BUILD_TARGET={target}");

    // Allow overriding the version via CRAB_BUILD_VERSION env var (e.g., from CI).
    // Falls back to CARGO_PKG_VERSION from Cargo.toml.
    let version = std::env::var("CRAB_BUILD_VERSION")
        .or_else(|_| std::env::var("CARGO_PKG_VERSION"))
        .unwrap_or_else(|_| "unknown".into());
    println!("cargo:rustc-env=CRAB_BUILD_VERSION={version}");

    let git_sha = std::env::var("GIT_SHA")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(git_short_sha)
        .unwrap_or_else(|| {
            println!(
                "cargo:warning=crab build.rs: could not resolve git sha, \
             using \"unknown\" (is git installed and is this a git checkout?)"
            );
            "unknown".into()
        });
    println!("cargo:rustc-env=CRAB_BUILD_GIT_SHA={git_sha}");

    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| format_epoch_utc(d.as_secs()))
        .unwrap_or_else(|_| "unknown".into());
    println!("cargo:rustc-env=CRAB_BUILD_TIMESTAMP={timestamp}");
}

// ── Pricing code generation ─────────────────────────────────────────

fn generate_pricing_embedded() {
    let yaml_path = PathBuf::from(format!("pricing/data/{PRICING_VERSION}.yaml"));
    if !yaml_path.exists() {
        panic!(
            "build.rs: pricing seed file not found at {path}",
            path = yaml_path.display()
        );
    }

    let yaml_content = fs::read_to_string(&yaml_path).unwrap_or_else(|e| {
        panic!(
            "build.rs: failed to read {path}: {e}",
            path = yaml_path.display()
        );
    });

    let pricing: PricingFile = serde_yaml::from_str(&yaml_content).unwrap_or_else(|e| {
        panic!(
            "build.rs: failed to parse {path}: {e}",
            path = yaml_path.display()
        );
    });

    if pricing.version != PRICING_VERSION {
        panic!(
            "build.rs: pricing file version mismatch: expected {PRICING_VERSION}, got {}",
            pricing.version
        );
    }

    validate_completeness(&pricing, &yaml_path);

    let out_dir = std::env::var("OUT_DIR").unwrap_or_else(|_| {
        panic!("build.rs: OUT_DIR not set");
    });
    let out_path = Path::new(&out_dir).join("pricing_embedded.rs");

    let generated = generate_rust_source(&pricing);
    fs::write(&out_path, generated).unwrap_or_else(|e| {
        panic!(
            "build.rs: failed to write {path}: {e}",
            path = out_path.display()
        );
    });
}

/// Validates that every `(provider, region, class)` tuple has all required
/// fields. Panics with a descriptive message on the first missing field.
fn validate_completeness(pricing: &PricingFile, yaml_path: &Path) {
    for (provider, prov_entry) in &pricing.providers {
        for (region, region_entry) in &prov_entry.regions {
            for (class, entry) in &region_entry.classes {
                validate_class_entry(provider, region, class, entry, yaml_path);
            }
        }
    }
}

fn validate_class_entry(
    provider: &str,
    region: &str,
    class: &str,
    entry: &ClassEntry,
    yaml_path: &Path,
) {
    // Build a map of field name → whether the value is null/missing.
    let fields: Vec<(&str, &serde_yaml::Value)> = vec![
        ("gb_month_usd", &entry.gb_month_usd),
        ("put_per_k_ops_usd", &entry.put_per_k_ops_usd),
        ("get_per_k_ops_usd", &entry.get_per_k_ops_usd),
        ("list_per_k_ops_usd", &entry.list_per_k_ops_usd),
        ("head_per_k_ops_usd", &entry.head_per_k_ops_usd),
        ("retrieval_per_gb_usd", &entry.retrieval_per_gb_usd),
        ("min_retention_days", &entry.min_retention_days),
        ("min_object_size_bytes", &entry.min_object_size_bytes),
        ("egress_per_gb_usd", &entry.egress_per_gb_usd),
    ];

    for (field_name, value) in &fields {
        if value.is_null() {
            panic!(
                "build.rs: {path}: ({provider}, {region}, {class}) is missing required field `{field_name}`",
                path = yaml_path.display(),
            );
        }
    }

    // Also verify that the required field names match what we expect.
    let field_names: Vec<&str> = fields.iter().map(|(name, _)| *name).collect();
    for required in REQUIRED_FIELDS {
        if !field_names.contains(required) {
            panic!(
                "build.rs: internal error — REQUIRED_FIELDS contains `{required}` \
                 but it is not checked in validate_class_entry"
            );
        }
    }
}

/// Extracts a string representation from a YAML value (handles both
/// quoted strings and bare numbers).
fn yaml_value_to_str(v: &serde_yaml::Value) -> String {
    match v {
        serde_yaml::Value::String(s) => s.clone(),
        serde_yaml::Value::Number(n) => n.to_string(),
        serde_yaml::Value::Bool(b) => b.to_string(),
        other => panic!("build.rs: unexpected YAML value type: {other:?}"),
    }
}

/// Extracts a u64 from a YAML value.
fn yaml_value_to_u64(v: &serde_yaml::Value) -> u64 {
    match v {
        serde_yaml::Value::Number(n) => n
            .as_u64()
            .unwrap_or_else(|| panic!("build.rs: expected unsigned integer, got {n}")),
        serde_yaml::Value::String(s) => s.parse::<u64>().unwrap_or_else(|e| {
            panic!("build.rs: expected unsigned integer string, got \"{s}\": {e}")
        }),
        other => panic!("build.rs: expected integer, got {other:?}"),
    }
}

fn rust_u64_literal(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    let first_group_len = digits.len() % 3;
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (index + 3 - first_group_len).is_multiple_of(3) {
            out.push('_');
        }
        out.push(ch);
    }
    out
}

/// Generates the Rust source for `pricing_embedded.rs`.
///
/// Produces a static data structure that can be queried at runtime by
/// `(provider, region, class)`. Uses nested arrays of tuples rather than
/// `phf` to keep build-time dependencies minimal.
fn generate_rust_source(pricing: &PricingFile) -> String {
    let mut out = String::with_capacity(8192);

    out.push_str("// @generated by build.rs from pricing/data/");
    out.push_str(&pricing.version);
    out.push_str(".yaml\n");
    out.push_str("// DO NOT EDIT — regenerate via `cargo build`.\n\n");

    out.push_str(&format!(
        "pub const PRICE_TABLE_VERSION: &str = {:?};\n\n",
        pricing.version
    ));

    // Generate a struct for each schedule entry.
    out.push_str(
        "\
/// A single pricing schedule for a `(provider, region, class)` tuple.
///
/// All USD fields are string representations of decimal values to
/// preserve precision. Convert to `rust_decimal::Decimal` at runtime.
#[derive(Debug, Clone, Copy)]
pub struct EmbeddedPriceSchedule {
    pub gb_month_usd: &'static str,
    pub put_per_k_ops_usd: &'static str,
    pub get_per_k_ops_usd: &'static str,
    pub list_per_k_ops_usd: &'static str,
    pub head_per_k_ops_usd: &'static str,
    pub retrieval_per_gb_usd: &'static str,
    pub min_retention_days: u32,
    pub min_object_size_bytes: u64,
    pub egress_per_gb_usd: &'static str,
}

/// A single entry in the embedded price table keyed by
/// `(provider, region, class)`.
#[derive(Debug, Clone, Copy)]
pub struct EmbeddedPriceEntry {
    pub provider: &'static str,
    pub region: &'static str,
    pub class: &'static str,
    pub schedule: EmbeddedPriceSchedule,
}

/// Returns the full embedded price table as a static slice.
///
/// Entries are sorted by `(provider, region, class)` for deterministic
/// output and binary-search friendliness.
pub fn embedded_price_table() -> &'static [EmbeddedPriceEntry] {
    PRICE_TABLE
}

/// Looks up a price schedule by `(provider, region, class)`.
///
/// Returns `None` if the tuple is not in the embedded table.
pub fn lookup_price(provider: &str, region: &str, class: &str) -> Option<&'static EmbeddedPriceSchedule> {
    PRICE_TABLE.iter().find(|e| {
        e.provider == provider && e.region == region && e.class == class
    }).map(|e| &e.schedule)
}

",
    );

    // Collect all entries sorted by (provider, region, class).
    let mut entries: Vec<(String, String, String, &ClassEntry)> = Vec::new();
    for (provider, prov_entry) in &pricing.providers {
        for (region, region_entry) in &prov_entry.regions {
            for (class, class_entry) in &region_entry.classes {
                entries.push((provider.clone(), region.clone(), class.clone(), class_entry));
            }
        }
    }
    entries.sort_by(|a, b| (&a.0, &a.1, &a.2).cmp(&(&b.0, &b.1, &b.2)));

    out.push_str("static PRICE_TABLE: &[EmbeddedPriceEntry] = &[\n");

    for (provider, region, class, entry) in &entries {
        out.push_str("    EmbeddedPriceEntry {\n");
        out.push_str(&format!("        provider: {provider:?},\n"));
        out.push_str(&format!("        region: {region:?},\n"));
        out.push_str(&format!("        class: {class:?},\n"));
        out.push_str("        schedule: EmbeddedPriceSchedule {\n");
        out.push_str(&format!(
            "            gb_month_usd: {:?},\n",
            yaml_value_to_str(&entry.gb_month_usd)
        ));
        out.push_str(&format!(
            "            put_per_k_ops_usd: {:?},\n",
            yaml_value_to_str(&entry.put_per_k_ops_usd)
        ));
        out.push_str(&format!(
            "            get_per_k_ops_usd: {:?},\n",
            yaml_value_to_str(&entry.get_per_k_ops_usd)
        ));
        out.push_str(&format!(
            "            list_per_k_ops_usd: {:?},\n",
            yaml_value_to_str(&entry.list_per_k_ops_usd)
        ));
        out.push_str(&format!(
            "            head_per_k_ops_usd: {:?},\n",
            yaml_value_to_str(&entry.head_per_k_ops_usd)
        ));
        out.push_str(&format!(
            "            retrieval_per_gb_usd: {:?},\n",
            yaml_value_to_str(&entry.retrieval_per_gb_usd)
        ));
        out.push_str(&format!(
            "            min_retention_days: {},\n",
            yaml_value_to_u64(&entry.min_retention_days)
        ));
        out.push_str(&format!(
            "            min_object_size_bytes: {},\n",
            rust_u64_literal(yaml_value_to_u64(&entry.min_object_size_bytes))
        ));
        out.push_str(&format!(
            "            egress_per_gb_usd: {:?},\n",
            yaml_value_to_str(&entry.egress_per_gb_usd)
        ));
        out.push_str("        },\n");
        out.push_str("    },\n");
    }

    out.push_str("];\n");
    out
}

// ── Git helpers ─────────────────────────────────────────────────────

/// Returns the short HEAD sha of the workspace, or `None` on any failure.
fn git_short_sha() -> Option<String> {
    let output = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let sha = String::from_utf8(output.stdout).ok()?.trim().to_string();
    if sha.is_empty() { None } else { Some(sha) }
}

/// Format a Unix epoch timestamp as `YYYY-MM-DD HH:MM:SS UTC`.
///
/// Pure arithmetic — no external crate needed at build time.
fn format_epoch_utc(epoch: u64) -> String {
    let mut remaining = epoch;
    let seconds = remaining % 60;
    remaining /= 60;
    let minutes = remaining % 60;
    remaining /= 60;
    let hours = remaining % 24;
    let mut days = remaining / 24;

    // Compute year from days since 1970-01-01.
    let mut year: u64 = 1970;
    loop {
        let ydays = if is_leap(year) { 366 } else { 365 };
        if days < ydays {
            break;
        }
        days -= ydays;
        year += 1;
    }

    let month_lengths: [u64; 12] = if is_leap(year) {
        [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };

    let mut month: u64 = 1;
    for &ml in &month_lengths {
        if days < ml {
            break;
        }
        days -= ml;
        month += 1;
    }
    let day = days + 1;

    format!("{year:04}-{month:02}-{day:02} {hours:02}:{minutes:02}:{seconds:02} UTC")
}

fn is_leap(y: u64) -> bool {
    y.is_multiple_of(4) && !y.is_multiple_of(100) || y.is_multiple_of(400)
}
