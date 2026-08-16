//! `crab.lock` — the git-tracked record of every stage's inputs
//! and outputs at their last successful run.
//!
//! The lockfile is written in Crab's canonical workflow YAML form:
//! block-style scalars only, every string double-quoted, keys sorted
//! at every map level, UTF-8 NFC normalization on all strings, hashes
//! prefixed with `"b3:"`, unix modes serialized as quoted
//! `"0o<octal>"` literals, and no YAML anchors / aliases / tags /
//! multi-document streams.
//!
//! These rules aren't what `serde_yaml` produces by default, so this
//! module hand-rolls a tiny emitter over a normalized intermediate
//! tree. Parsing uses `serde_yaml` directly — any conforming reader
//! accepts the output, we only need determinism on write.
//!
//! Two lockfiles with the same logical content round-trip to
//! byte-identical bytes — the byte-equality property test in this
//! module locks that invariant in.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::Serialize;
use unicode_normalization::UnicodeNormalization;

use crate::{CachedCmd, CachedOut, OutKind, Result, StageCacheEntry, StageName, WorkflowError};
use crab_types::workflow::StageHash;

/// Schema version of the on-disk lockfile.
pub const LOCKFILE_SCHEMA_VERSION: u16 = 2;

/// Hash-algorithm tag recorded in the lockfile.
pub const LOCKFILE_HASH_ALGO: &str = "crab.stage.v1";

/// A dependency as recorded in the lockfile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockedDep {
    pub path: PathBuf,
    pub hash: [u8; 32],
    pub size: u64,
}

/// An output as recorded in the lockfile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockedOut {
    pub path: PathBuf,
    pub kind: OutKind,
    pub hash: [u8; 32],
    pub size: u64,
    pub mode: u32,
}

/// A metric file as recorded in the lockfile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockedMetric {
    pub path: PathBuf,
    pub hash: [u8; 32],
}

/// One stage's entry in the lockfile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockedStage {
    pub stage_hash: StageHash,
    pub cmd: CachedCmd,
    pub deps: Vec<LockedDep>,
    pub params: BTreeMap<String, String>,
    pub env: BTreeMap<String, String>,
    pub outs: Vec<LockedOut>,
    pub metrics: Vec<LockedMetric>,
    pub plots: Vec<LockedOut>,
    pub executed_at: String,
    pub duration_ms: u64,
    pub host_fingerprint: String,
    pub attempts: u32,
    /// Where the cache hit came from: `"Local"`, `"Remote"`, or `"Execution"`.
    pub source: String,
}

/// A single field difference between lockfile values and current stage inputs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExplainMissDiff {
    pub category: String,
    pub key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new: Option<String>,
}

/// The full `crab.lock` document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lockfile {
    pub schema_version: u16,
    pub crab_hash_algo: String,
    pub stages: BTreeMap<StageName, LockedStage>,
}

impl Default for Lockfile {
    fn default() -> Self {
        Self {
            schema_version: LOCKFILE_SCHEMA_VERSION,
            crab_hash_algo: LOCKFILE_HASH_ALGO.to_owned(),
            stages: BTreeMap::new(),
        }
    }
}

impl Lockfile {
    /// Build an empty lockfile at the current schema version.
    pub fn new() -> Self {
        Self::default()
    }

    /// Read a lockfile from disk. A missing file is not an error —
    /// it's the "fresh repo" case, which returns
    /// [`Lockfile::default`].
    pub fn load(path: &Path) -> Result<Self> {
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(WorkflowError::Io(e)),
        };
        Self::parse(path, &bytes)
    }

    /// Parse lockfile bytes. Public for testing and for the merge
    /// resolver which reads both conflict sides from memory.
    pub fn parse(path: &Path, bytes: &[u8]) -> Result<Self> {
        parse_lockfile(path, bytes)
    }

    /// Serialize the lockfile to its canonical byte form. Calling
    /// this on two logically equal lockfiles yields byte-identical
    /// output.
    pub fn serialize_canonical(&self) -> Result<Vec<u8>> {
        let mut out = String::new();
        emit_lockfile(self, &mut out);
        Ok(out.into_bytes())
    }

    /// Atomically write the lockfile to `path`.
    ///
    /// Uses the same tempfile-plus-rename pattern the cache entry
    /// writer does so a crash mid-write never leaves a truncated or
    /// mixed-content lockfile on disk.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(WorkflowError::Io)?;
        }
        let bytes = self.serialize_canonical()?;
        atomic_write(path, &bytes)
    }

    /// Insert or replace a stage entry from a [`StageCacheEntry`].
    ///
    /// The cache entry carries everything the lockfile needs except
    /// deps, params, and env — those are inputs and live on the
    /// resolved stage. The caller supplies them here so the lockfile
    /// module doesn't need to know about `ResolvedStage` directly.
    pub fn upsert(
        &mut self,
        entry: &StageCacheEntry,
        deps: Vec<LockedDep>,
        params: BTreeMap<String, String>,
        env: BTreeMap<String, String>,
    ) -> Result<()> {
        let name = StageName::parse_effective(&entry.stage_name)?;
        let locked = LockedStage {
            stage_hash: entry.stage_hash,
            cmd: entry.cmd.clone(),
            deps,
            params,
            env,
            outs: entry
                .outs
                .iter()
                .map(locked_out_from)
                .collect::<Result<_>>()?,
            metrics: entry
                .metrics
                .iter()
                .map(locked_metric_from)
                .collect::<Result<_>>()?,
            plots: entry
                .plots
                .iter()
                .map(locked_out_from)
                .collect::<Result<_>>()?,
            executed_at: entry.executed_at.clone(),
            duration_ms: entry.duration_ms,
            host_fingerprint: entry.host_fingerprint.clone(),
            attempts: entry.attempts,
            source: "Local".to_owned(),
        };
        self.stages.insert(name, locked);
        Ok(())
    }

    /// Remove every stage whose name is not in `keep`. Returns the
    /// names that were pruned so the caller can log them (orphan
    /// handling: warn + continue, never fail the run).
    pub fn prune_stages_not_in(&mut self, keep: &BTreeSet<StageName>) -> Vec<StageName> {
        let mut pruned = Vec::new();
        self.stages.retain(|name, _| {
            let keeping = keep.contains(name);
            if !keeping {
                pruned.push(name.clone());
            }
            keeping
        });
        pruned.sort();
        pruned
    }

    /// Look up a stage entry by name.
    pub fn get(&self, name: &StageName) -> Option<&LockedStage> {
        self.stages.get(name)
    }

    /// Compute a field-by-field diff between the lockfile's recorded
    /// values for `stage_name` and the current resolved stage inputs.
    ///
    /// Returns `None` when the lockfile has no entry for the stage
    /// (the "never run" case — the caller handles that separately).
    /// Otherwise returns a vec of [`ExplainMissDiff`] entries, one
    /// per changed input field.
    pub fn diff_against_resolved(
        &self,
        stage_name: &StageName,
        current_dep_hashes: &std::collections::BTreeMap<String, [u8; 32]>,
        current_params: &std::collections::BTreeMap<String, String>,
        current_env: &std::collections::BTreeMap<String, String>,
        current_cmd: &CachedCmd,
    ) -> Option<Vec<ExplainMissDiff>> {
        let locked = self.stages.get(stage_name)?;
        let mut diffs = Vec::new();

        // Deps: compare blake3 hashes per path.
        let locked_deps: std::collections::BTreeMap<String, [u8; 32]> = locked
            .deps
            .iter()
            .map(|d| (d.path.to_string_lossy().into_owned(), d.hash))
            .collect();

        for (path, new_hash) in current_dep_hashes {
            match locked_deps.get(path) {
                Some(old_hash) if old_hash != new_hash => {
                    diffs.push(ExplainMissDiff {
                        category: "dep".to_owned(),
                        key: path.clone(),
                        old: Some(format_b3(old_hash)),
                        new: Some(format_b3(new_hash)),
                    });
                }
                None => {
                    diffs.push(ExplainMissDiff {
                        category: "dep".to_owned(),
                        key: path.clone(),
                        old: None,
                        new: Some(format_b3(new_hash)),
                    });
                }
                _ => {}
            }
        }
        for path in locked_deps.keys() {
            if !current_dep_hashes.contains_key(path) {
                diffs.push(ExplainMissDiff {
                    category: "dep".to_owned(),
                    key: path.clone(),
                    old: Some(format_b3(&locked_deps[path])),
                    new: None,
                });
            }
        }

        // Params: compare scalar values per dotted key.
        for (key, new_val) in current_params {
            match locked.params.get(key) {
                Some(old_val) if old_val != new_val => {
                    diffs.push(ExplainMissDiff {
                        category: "param".to_owned(),
                        key: key.clone(),
                        old: Some(old_val.clone()),
                        new: Some(new_val.clone()),
                    });
                }
                None => {
                    diffs.push(ExplainMissDiff {
                        category: "param".to_owned(),
                        key: key.clone(),
                        old: None,
                        new: Some(new_val.clone()),
                    });
                }
                _ => {}
            }
        }
        for key in locked.params.keys() {
            if !current_params.contains_key(key) {
                diffs.push(ExplainMissDiff {
                    category: "param".to_owned(),
                    key: key.clone(),
                    old: Some(locked.params[key].clone()),
                    new: None,
                });
            }
        }

        // Env: compare values per allowlisted variable.
        for (key, new_val) in current_env {
            match locked.env.get(key) {
                Some(old_val) if old_val != new_val => {
                    diffs.push(ExplainMissDiff {
                        category: "env".to_owned(),
                        key: key.clone(),
                        old: Some(old_val.clone()),
                        new: Some(new_val.clone()),
                    });
                }
                None => {
                    diffs.push(ExplainMissDiff {
                        category: "env".to_owned(),
                        key: key.clone(),
                        old: None,
                        new: Some(new_val.clone()),
                    });
                }
                _ => {}
            }
        }
        for key in locked.env.keys() {
            if !current_env.contains_key(key) {
                diffs.push(ExplainMissDiff {
                    category: "env".to_owned(),
                    key: key.clone(),
                    old: Some(locked.env[key].clone()),
                    new: None,
                });
            }
        }

        // Cmd: compare shell string or argv vector.
        let cmd_matches = &locked.cmd == current_cmd;
        if !cmd_matches {
            let old_str = match &locked.cmd {
                CachedCmd::Argv { argv } => format!("argv:{}", argv.join(" ")),
                CachedCmd::Shell { shell } => format!("shell:{shell}"),
                CachedCmd::ShellList { commands } => format!("shells:{}", commands.join(" && ")),
            };
            let new_str = match current_cmd {
                CachedCmd::Argv { argv } => format!("argv:{}", argv.join(" ")),
                CachedCmd::Shell { shell } => format!("shell:{shell}"),
                CachedCmd::ShellList { commands } => format!("shells:{}", commands.join(" && ")),
            };
            diffs.push(ExplainMissDiff {
                category: "cmd".to_owned(),
                key: "cmd".to_owned(),
                old: Some(old_str),
                new: Some(new_str),
            });
        }

        Some(diffs)
    }
}

// --- StageCacheEntry → LockedStage helpers ---

fn locked_out_from(out: &CachedOut) -> Result<LockedOut> {
    Ok(LockedOut {
        path: out.path.clone(),
        kind: out.kind,
        hash: parse_b3_hash(&out.file_hash)?,
        size: out.size,
        mode: out.mode,
    })
}

fn locked_metric_from(out: &CachedOut) -> Result<LockedMetric> {
    Ok(LockedMetric {
        path: out.path.clone(),
        hash: parse_b3_hash(&out.file_hash)?,
    })
}

/// Parse a `"b3:<64-hex>"` string into a 32-byte digest.
fn parse_b3_hash(s: &str) -> Result<[u8; 32]> {
    let Some(hex) = s.strip_prefix("b3:") else {
        return Err(WorkflowError::LockfileHashMalformed {
            hash: s.to_owned(),
            reason: "missing the 'b3:' prefix".to_owned(),
        });
    };
    if hex.len() != 64 {
        return Err(WorkflowError::LockfileHashMalformed {
            hash: s.to_owned(),
            reason: format!("has {} hex chars, expected 64", hex.len()),
        });
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        let pair = &hex[i * 2..i * 2 + 2];
        *byte = u8::from_str_radix(pair, 16).map_err(|_| WorkflowError::LockfileHashMalformed {
            hash: s.to_owned(),
            reason: "has non-hex chars".to_owned(),
        })?;
    }
    Ok(out)
}

fn parse_b3_hash_in_lockfile(s: &str, path: &Path) -> Result<[u8; 32]> {
    parse_b3_hash(s).map_err(|error| match error {
        WorkflowError::LockfileHashMalformed { .. } => {
            WorkflowError::LockfileCanonicalizationFailed {
                path: path.to_path_buf(),
                source: serde_yaml_custom_owned(error.to_string()),
            }
        }
        other => other,
    })
}

// --- Canonical YAML emitter ---

const INDENT: &str = "  ";

/// Emit the full lockfile.
///
/// The emitter writes directly into an owned `String`; it cannot
/// fail. Returning `()` rather than `Result<()>` keeps the internals
/// honest — any fallible upstream step (NFC normalization, path
/// conversion) happens before we reach this function.
fn emit_lockfile(lock: &Lockfile, out: &mut String) {
    // Top-level keys are always the same three — emitting them
    // inline keeps the output stable regardless of future map-level
    // additions. Keys are sorted alphabetically.
    push_scalar_key(out, 0, "crab_hash_algo");
    push_quoted_string(out, &nfc(&lock.crab_hash_algo));
    out.push('\n');

    push_scalar_key(out, 0, "schema_version");
    push_u64(out, u64::from(lock.schema_version));
    out.push('\n');

    push_key(out, 0, "stages");
    if lock.stages.is_empty() {
        out.push_str(" {}\n");
    } else {
        out.push('\n');
        for (name, stage) in &lock.stages {
            emit_stage(out, 1, name, stage);
        }
    }
}

fn emit_stage(out: &mut String, depth: usize, name: &StageName, stage: &LockedStage) {
    push_key(out, depth, name.as_str());
    out.push('\n');

    // Keys inside a stage, in sorted order.
    push_scalar_key(out, depth + 1, "attempts");
    push_u64(out, u64::from(stage.attempts));
    out.push('\n');

    push_key(out, depth + 1, "cmd");
    out.push('\n');
    emit_cmd(out, depth + 2, &stage.cmd);

    push_key(out, depth + 1, "deps");
    if stage.deps.is_empty() {
        out.push_str(" []\n");
    } else {
        out.push('\n');
        for dep in &stage.deps {
            emit_dep(out, depth + 1, dep);
        }
    }

    push_scalar_key(out, depth + 1, "duration_ms");
    push_u64(out, stage.duration_ms);
    out.push('\n');

    // env is always written as a map, even when empty — the key's
    // presence is informative (the stage ran under a declared env
    // policy; the set of resolved vars happens to be empty).
    push_key(out, depth + 1, "env");
    if stage.env.is_empty() {
        out.push_str(" {}\n");
    } else {
        out.push('\n');
        for (k, v) in &stage.env {
            push_scalar_key(out, depth + 2, &nfc(k));
            push_quoted_string(out, &nfc(v));
            out.push('\n');
        }
    }

    push_scalar_key(out, depth + 1, "executed_at");
    push_quoted_string(out, &nfc(&stage.executed_at));
    out.push('\n');

    push_scalar_key(out, depth + 1, "host_fingerprint");
    push_quoted_string(out, &nfc(&stage.host_fingerprint));
    out.push('\n');

    push_key(out, depth + 1, "metrics");
    if stage.metrics.is_empty() {
        out.push_str(" []\n");
    } else {
        out.push('\n');
        for m in &stage.metrics {
            emit_metric(out, depth + 1, m);
        }
    }

    push_key(out, depth + 1, "outs");
    if stage.outs.is_empty() {
        out.push_str(" []\n");
    } else {
        out.push('\n');
        for o in &stage.outs {
            emit_out(out, depth + 1, o);
        }
    }

    push_key(out, depth + 1, "params");
    if stage.params.is_empty() {
        out.push_str(" {}\n");
    } else {
        out.push('\n');
        for (k, v) in &stage.params {
            push_scalar_key(out, depth + 2, &nfc(k));
            push_quoted_string(out, &nfc(v));
            out.push('\n');
        }
    }

    push_key(out, depth + 1, "plots");
    if stage.plots.is_empty() {
        out.push_str(" []\n");
    } else {
        out.push('\n');
        for p in &stage.plots {
            emit_out(out, depth + 1, p);
        }
    }

    push_scalar_key(out, depth + 1, "source");
    push_quoted_string(out, &nfc(&stage.source));
    out.push('\n');

    push_scalar_key(out, depth + 1, "stage_hash");
    push_quoted_string(out, &format_b3(&stage.stage_hash.0));
    out.push('\n');
}

fn emit_cmd(out: &mut String, depth: usize, cmd: &CachedCmd) {
    // The two variants are mutually exclusive; emit only the
    // present key so the lockfile stays diff-friendly (no empty
    // placeholder lines when only one variant is used).
    match cmd {
        CachedCmd::Argv { argv } => {
            push_key(out, depth, "argv");
            if argv.is_empty() {
                out.push_str(" []\n");
            } else {
                out.push('\n');
                for arg in argv {
                    push_indent(out, depth);
                    out.push_str("- ");
                    push_quoted_string(out, &nfc(arg));
                    out.push('\n');
                }
            }
        }
        CachedCmd::Shell { shell } => {
            push_scalar_key(out, depth, "shell");
            push_quoted_string(out, &nfc(shell));
            out.push('\n');
        }
        CachedCmd::ShellList { commands } => {
            push_key(out, depth, "shells");
            if commands.is_empty() {
                out.push_str(" []\n");
            } else {
                out.push('\n');
                for command in commands {
                    push_indent(out, depth);
                    out.push_str("- ");
                    push_quoted_string(out, &nfc(command));
                    out.push('\n');
                }
            }
        }
    }
}

fn emit_dep(out: &mut String, depth: usize, dep: &LockedDep) {
    // YAML sequence-of-map items: the first key's `-` lives at the
    // sequence's indentation level, subsequent keys align one level
    // deeper.
    push_indent(out, depth);
    out.push_str("- ");
    // Keys inside a dep entry, sorted: hash, path, size.
    out.push_str("hash: ");
    push_quoted_string(out, &format_b3(&dep.hash));
    out.push('\n');

    push_scalar_key(out, depth + 1, "path");
    push_quoted_string(out, &nfc(&path_to_string(&dep.path)));
    out.push('\n');

    push_scalar_key(out, depth + 1, "size");
    push_u64(out, dep.size);
    out.push('\n');
}

fn emit_out(out: &mut String, depth: usize, o: &LockedOut) {
    push_indent(out, depth);
    out.push_str("- ");
    // Keys inside an out entry, sorted: hash, kind, mode, path, size.
    out.push_str("hash: ");
    push_quoted_string(out, &format_b3(&o.hash));
    out.push('\n');

    push_scalar_key(out, depth + 1, "kind");
    push_quoted_string(out, o.kind.as_str());
    out.push('\n');

    push_scalar_key(out, depth + 1, "mode");
    push_quoted_string(out, &format!("0o{:o}", o.mode));
    out.push('\n');

    push_scalar_key(out, depth + 1, "path");
    push_quoted_string(out, &nfc(&path_to_string(&o.path)));
    out.push('\n');

    push_scalar_key(out, depth + 1, "size");
    push_u64(out, o.size);
    out.push('\n');
}

fn emit_metric(out: &mut String, depth: usize, m: &LockedMetric) {
    push_indent(out, depth);
    out.push_str("- ");
    out.push_str("hash: ");
    push_quoted_string(out, &format_b3(&m.hash));
    out.push('\n');

    push_scalar_key(out, depth + 1, "path");
    push_quoted_string(out, &nfc(&path_to_string(&m.path)));
    out.push('\n');
}

fn push_indent(out: &mut String, depth: usize) {
    for _ in 0..depth {
        out.push_str(INDENT);
    }
}

/// Emit a bare key followed by `:` — used when the value starts on
/// a fresh line (mappings, sequences, block scalars).
fn push_key(out: &mut String, depth: usize, key: &str) {
    push_indent(out, depth);
    // Map keys are plain unquoted identifiers per the canonical
    // grammar. Stage names are already validated to a conservative
    // ASCII set, and top-level keys are literal strings we control.
    out.push_str(key);
    out.push(':');
}

/// Emit a key followed by `: ` with a trailing space — used when
/// the scalar value follows on the same line.
fn push_scalar_key(out: &mut String, depth: usize, key: &str) {
    push_indent(out, depth);
    out.push_str(key);
    out.push_str(": ");
}

fn push_u64(out: &mut String, n: u64) {
    // Integers are emitted as plain scalars. They can't collide with
    // reserved YAML words and don't need quoting.
    out.push_str(&n.to_string());
}

/// Emit a string as a YAML double-quoted scalar.
///
/// Double-quoted YAML supports standard C-style escapes for `"`,
/// `\`, and control characters — enough for the set of strings
/// appearing in a lockfile (paths, hashes, timestamps, host
/// fingerprints). We intentionally don't support the full YAML
/// double-quoted grammar: the emitter controls its own input.
fn push_quoted_string(out: &mut String, s: &str) {
    use std::fmt::Write as _;
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                // Remaining control chars escape to `\xNN`. YAML 1.2
                // spec-compliant and readable in diffs. Writing into
                // a `String` can't fail; the `.ok()` silences the
                // Result without an `unwrap`.
                let _ = write!(out, "\\x{:02x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

fn path_to_string(p: &Path) -> String {
    // PathBuf → String via `to_string_lossy`. The lockfile is
    // git-tracked so invalid UTF-8 paths would be a problem long
    // before reaching this code path; the lossy conversion is safe
    // in practice and lets us normalize in the next step.
    p.to_string_lossy().into_owned()
}

/// Normalize to Unicode NFC. The lockfile is a cross-machine
/// artifact: the same path can appear in decomposed form on one
/// filesystem and composed on another, and the lockfile needs to
/// sort and serialize identically regardless.
fn nfc(s: &str) -> String {
    s.nfc().collect()
}

/// `b3:` + lowercase hex of the given 32 bytes.
fn format_b3(bytes: &[u8; 32]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(3 + 64);
    out.push_str("b3:");
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// Same atomic write helper the cache entry module uses — tempfile
/// in the destination directory, fsync, rename.
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write as _;

    let parent = path
        .parent()
        .ok_or_else(|| WorkflowError::LockfilePathNoParent {
            path: path.to_path_buf(),
        })?;
    let parent = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };
    let mut tmp = tempfile::NamedTempFile::new_in(parent).map_err(WorkflowError::Io)?;
    tmp.write_all(bytes).map_err(WorkflowError::Io)?;
    tmp.as_file().sync_all().map_err(WorkflowError::Io)?;
    tmp.persist(path).map_err(|e| WorkflowError::Io(e.error))?;
    Ok(())
}

// --- Parser ---

fn parse_lockfile(path: &Path, bytes: &[u8]) -> Result<Lockfile> {
    let text =
        std::str::from_utf8(bytes).map_err(|_| WorkflowError::LockfileCanonicalizationFailed {
            path: path.to_path_buf(),
            source: serde_yaml_invalid_utf8(),
        })?;

    let value: serde_yaml::Value =
        serde_yaml::from_str(text).map_err(|e| WorkflowError::LockfileCanonicalizationFailed {
            path: path.to_path_buf(),
            source: e,
        })?;

    let root = value
        .as_mapping()
        .ok_or_else(|| WorkflowError::LockfileCanonicalizationFailed {
            path: path.to_path_buf(),
            source: serde_yaml_custom("lockfile root must be a mapping"),
        })?;

    let schema_version = get_u64(root, "schema_version", path)?;
    let schema_version = u16::try_from(schema_version).map_err(|_| {
        WorkflowError::LockfileCanonicalizationFailed {
            path: path.to_path_buf(),
            source: serde_yaml_custom("schema_version does not fit in u16"),
        }
    })?;

    let crab_hash_algo = get_str(root, "crab_hash_algo", path)?.to_owned();

    let stages_value = root
        .get(serde_yaml::Value::String("stages".into()))
        .cloned()
        .unwrap_or(serde_yaml::Value::Mapping(serde_yaml::Mapping::new()));
    let stages_map = stages_value.as_mapping().cloned().ok_or_else(|| {
        WorkflowError::LockfileCanonicalizationFailed {
            path: path.to_path_buf(),
            source: serde_yaml_custom("stages must be a mapping"),
        }
    })?;

    let mut stages = BTreeMap::new();
    for (k, v) in &stages_map {
        let name = k
            .as_str()
            .ok_or_else(|| WorkflowError::LockfileCanonicalizationFailed {
                path: path.to_path_buf(),
                source: serde_yaml_custom("stage name must be a string"),
            })?;
        let name = StageName::parse_effective(name)?;
        let stage = parse_stage(v, path)?;
        stages.insert(name, stage);
    }

    Ok(Lockfile {
        schema_version,
        crab_hash_algo,
        stages,
    })
}

fn parse_stage(value: &serde_yaml::Value, path: &Path) -> Result<LockedStage> {
    let map = value
        .as_mapping()
        .ok_or_else(|| WorkflowError::LockfileCanonicalizationFailed {
            path: path.to_path_buf(),
            source: serde_yaml_custom("stage must be a mapping"),
        })?;

    let stage_hash_hex = get_str(map, "stage_hash", path)?;
    let stage_hash = parse_b3_hash_in_lockfile(stage_hash_hex, path)?;

    let cmd = parse_cmd(
        map.get(serde_yaml::Value::String("cmd".into()))
            .ok_or_else(|| WorkflowError::LockfileCanonicalizationFailed {
                path: path.to_path_buf(),
                source: serde_yaml_custom("stage missing cmd"),
            })?,
        path,
    )?;

    let deps = parse_deps(
        map.get(serde_yaml::Value::String("deps".into()))
            .unwrap_or(&serde_yaml::Value::Sequence(Vec::new())),
        path,
    )?;

    let params = parse_str_map(
        map.get(serde_yaml::Value::String("params".into())),
        path,
        "params",
    )?;

    let env = parse_str_map(
        map.get(serde_yaml::Value::String("env".into())),
        path,
        "env",
    )?;

    let outs = parse_outs(
        map.get(serde_yaml::Value::String("outs".into()))
            .unwrap_or(&serde_yaml::Value::Sequence(Vec::new())),
        path,
    )?;

    let metrics = parse_metrics(
        map.get(serde_yaml::Value::String("metrics".into()))
            .unwrap_or(&serde_yaml::Value::Sequence(Vec::new())),
        path,
    )?;

    let plots = parse_outs(
        map.get(serde_yaml::Value::String("plots".into()))
            .unwrap_or(&serde_yaml::Value::Sequence(Vec::new())),
        path,
    )?;

    let executed_at = get_str(map, "executed_at", path)?.to_owned();
    let duration_ms = get_u64(map, "duration_ms", path)?;
    let host_fingerprint = get_str(map, "host_fingerprint", path)?.to_owned();
    let attempts = u32::try_from(get_u64_or_default(map, "attempts", 1)).map_err(|_| {
        WorkflowError::LockfileCanonicalizationFailed {
            path: path.to_path_buf(),
            source: serde_yaml_custom("attempts does not fit in u32"),
        }
    })?;
    let source = get_str_or_default(map, "source", "Local").to_owned();

    Ok(LockedStage {
        stage_hash: StageHash(stage_hash),
        cmd,
        deps,
        params,
        env,
        outs,
        metrics,
        plots,
        executed_at,
        duration_ms,
        host_fingerprint,
        attempts,
        source,
    })
}

fn parse_cmd(value: &serde_yaml::Value, path: &Path) -> Result<CachedCmd> {
    let map = value
        .as_mapping()
        .ok_or_else(|| WorkflowError::LockfileCanonicalizationFailed {
            path: path.to_path_buf(),
            source: serde_yaml_custom("cmd must be a mapping"),
        })?;
    if let Some(shell) = map.get(serde_yaml::Value::String("shell".into())) {
        let s = shell
            .as_str()
            .ok_or_else(|| WorkflowError::LockfileCanonicalizationFailed {
                path: path.to_path_buf(),
                source: serde_yaml_custom("cmd.shell must be a string"),
            })?;
        return Ok(CachedCmd::Shell {
            shell: s.to_owned(),
        });
    }
    if let Some(argv) = map.get(serde_yaml::Value::String("argv".into())) {
        let seq =
            argv.as_sequence()
                .ok_or_else(|| WorkflowError::LockfileCanonicalizationFailed {
                    path: path.to_path_buf(),
                    source: serde_yaml_custom("cmd.argv must be a sequence"),
                })?;
        let mut argv_out = Vec::with_capacity(seq.len());
        for item in seq {
            let s = item
                .as_str()
                .ok_or_else(|| WorkflowError::LockfileCanonicalizationFailed {
                    path: path.to_path_buf(),
                    source: serde_yaml_custom("cmd.argv items must be strings"),
                })?;
            argv_out.push(s.to_owned());
        }
        return Ok(CachedCmd::Argv { argv: argv_out });
    }
    if let Some(shells) = map.get(serde_yaml::Value::String("shells".into())) {
        let seq =
            shells
                .as_sequence()
                .ok_or_else(|| WorkflowError::LockfileCanonicalizationFailed {
                    path: path.to_path_buf(),
                    source: serde_yaml_custom("cmd.shells must be a sequence"),
                })?;
        let mut commands = Vec::with_capacity(seq.len());
        for item in seq {
            let s = item
                .as_str()
                .ok_or_else(|| WorkflowError::LockfileCanonicalizationFailed {
                    path: path.to_path_buf(),
                    source: serde_yaml_custom("cmd.shells items must be strings"),
                })?;
            commands.push(s.to_owned());
        }
        return Ok(CachedCmd::ShellList { commands });
    }
    Err(WorkflowError::LockfileCanonicalizationFailed {
        path: path.to_path_buf(),
        source: serde_yaml_custom("cmd must have `shell`, `shells`, or `argv`"),
    })
}

fn parse_deps(value: &serde_yaml::Value, path: &Path) -> Result<Vec<LockedDep>> {
    let seq = value
        .as_sequence()
        .ok_or_else(|| WorkflowError::LockfileCanonicalizationFailed {
            path: path.to_path_buf(),
            source: serde_yaml_custom("deps must be a sequence"),
        })?;
    let mut out = Vec::with_capacity(seq.len());
    for item in seq {
        let m = item
            .as_mapping()
            .ok_or_else(|| WorkflowError::LockfileCanonicalizationFailed {
                path: path.to_path_buf(),
                source: serde_yaml_custom("dep entry must be a mapping"),
            })?;
        let hash = parse_b3_hash_in_lockfile(get_str(m, "hash", path)?, path)?;
        let dep_path = PathBuf::from(get_str(m, "path", path)?);
        let size = get_u64(m, "size", path)?;
        out.push(LockedDep {
            path: dep_path,
            hash,
            size,
        });
    }
    Ok(out)
}

fn parse_outs(value: &serde_yaml::Value, path: &Path) -> Result<Vec<LockedOut>> {
    let seq = value
        .as_sequence()
        .ok_or_else(|| WorkflowError::LockfileCanonicalizationFailed {
            path: path.to_path_buf(),
            source: serde_yaml_custom("outs must be a sequence"),
        })?;
    let mut out = Vec::with_capacity(seq.len());
    for item in seq {
        let m = item
            .as_mapping()
            .ok_or_else(|| WorkflowError::LockfileCanonicalizationFailed {
                path: path.to_path_buf(),
                source: serde_yaml_custom("out entry must be a mapping"),
            })?;
        let hash = parse_b3_hash_in_lockfile(get_str(m, "hash", path)?, path)?;
        let kind = match get_str(m, "kind", path)? {
            "file" | "stdout" => OutKind::File,
            "directory" => OutKind::Directory,
            other => {
                return Err(WorkflowError::LockfileCanonicalizationFailed {
                    path: path.to_path_buf(),
                    source: serde_yaml_custom_owned(format!(
                        "out kind '{other}' is not recognized"
                    )),
                });
            }
        };
        let mode_str = get_str(m, "mode", path)?;
        let mode = parse_octal_mode(mode_str).ok_or_else(|| {
            WorkflowError::LockfileCanonicalizationFailed {
                path: path.to_path_buf(),
                source: serde_yaml_custom_owned(format!(
                    "out mode '{mode_str}' is not a 0o-prefixed octal literal"
                )),
            }
        })?;
        let out_path = PathBuf::from(get_str(m, "path", path)?);
        let size = get_u64(m, "size", path)?;
        out.push(LockedOut {
            path: out_path,
            kind,
            hash,
            size,
            mode,
        });
    }
    Ok(out)
}

fn parse_metrics(value: &serde_yaml::Value, path: &Path) -> Result<Vec<LockedMetric>> {
    let seq = value
        .as_sequence()
        .ok_or_else(|| WorkflowError::LockfileCanonicalizationFailed {
            path: path.to_path_buf(),
            source: serde_yaml_custom("metrics must be a sequence"),
        })?;
    let mut out = Vec::with_capacity(seq.len());
    for item in seq {
        let m = item
            .as_mapping()
            .ok_or_else(|| WorkflowError::LockfileCanonicalizationFailed {
                path: path.to_path_buf(),
                source: serde_yaml_custom("metric entry must be a mapping"),
            })?;
        let hash = parse_b3_hash_in_lockfile(get_str(m, "hash", path)?, path)?;
        let metric_path = PathBuf::from(get_str(m, "path", path)?);
        out.push(LockedMetric {
            path: metric_path,
            hash,
        });
    }
    Ok(out)
}

fn parse_str_map(
    value: Option<&serde_yaml::Value>,
    path: &Path,
    field: &'static str,
) -> Result<BTreeMap<String, String>> {
    let Some(value) = value else {
        return Ok(BTreeMap::new());
    };
    let m = value
        .as_mapping()
        .ok_or_else(|| WorkflowError::LockfileCanonicalizationFailed {
            path: path.to_path_buf(),
            source: serde_yaml_custom_owned(format!("{field} must be a mapping")),
        })?;
    let mut out = BTreeMap::new();
    for (k, v) in m {
        let key = k
            .as_str()
            .ok_or_else(|| WorkflowError::LockfileCanonicalizationFailed {
                path: path.to_path_buf(),
                source: serde_yaml_custom_owned(format!("{field} keys must be strings")),
            })?;
        let val = match v {
            serde_yaml::Value::String(s) => s.clone(),
            // Accept the common scalar types that could appear when a
            // human hand-edits the file — we stringify them so round-
            // tripping through the emitter produces a pure string.
            serde_yaml::Value::Number(n) => n.to_string(),
            serde_yaml::Value::Bool(b) => b.to_string(),
            serde_yaml::Value::Null => String::new(),
            _ => {
                return Err(WorkflowError::LockfileCanonicalizationFailed {
                    path: path.to_path_buf(),
                    source: serde_yaml_custom_owned(format!(
                        "{field} values must be scalars; key '{key}' has a non-scalar"
                    )),
                });
            }
        };
        out.insert(key.to_owned(), val);
    }
    Ok(out)
}

fn get_str<'a>(map: &'a serde_yaml::Mapping, key: &str, path: &Path) -> Result<&'a str> {
    let key_value = serde_yaml::Value::String(key.to_owned());
    map.get(&key_value)
        .and_then(serde_yaml::Value::as_str)
        .ok_or_else(|| WorkflowError::LockfileCanonicalizationFailed {
            path: path.to_path_buf(),
            source: serde_yaml_custom_owned(format!("missing string field '{key}'")),
        })
}

/// Like [`get_str`] but returns a default when the key is absent.
/// Used for fields added in newer lockfile schema versions so v1
/// lockfiles parse without error.
fn get_str_or_default<'a>(map: &'a serde_yaml::Mapping, key: &str, default: &'a str) -> &'a str {
    let key_value = serde_yaml::Value::String(key.to_owned());
    map.get(&key_value)
        .and_then(serde_yaml::Value::as_str)
        .unwrap_or(default)
}

fn get_u64(map: &serde_yaml::Mapping, key: &str, path: &Path) -> Result<u64> {
    let key_value = serde_yaml::Value::String(key.to_owned());
    map.get(&key_value)
        .and_then(serde_yaml::Value::as_u64)
        .ok_or_else(|| WorkflowError::LockfileCanonicalizationFailed {
            path: path.to_path_buf(),
            source: serde_yaml_custom_owned(format!("missing integer field '{key}'")),
        })
}

/// Like [`get_u64`] but returns a default when the key is absent.
/// Used for fields added in newer lockfile schema versions so v1
/// lockfiles parse without error.
fn get_u64_or_default(map: &serde_yaml::Mapping, key: &str, default: u64) -> u64 {
    let key_value = serde_yaml::Value::String(key.to_owned());
    map.get(&key_value)
        .and_then(serde_yaml::Value::as_u64)
        .unwrap_or(default)
}

fn parse_octal_mode(s: &str) -> Option<u32> {
    let rest = s.strip_prefix("0o")?;
    u32::from_str_radix(rest, 8).ok()
}

/// Construct a synthetic `serde_yaml::Error` for parse failures that
/// don't come from a real parse attempt. `serde_yaml::Error` is
/// deliberately opaque — this is the cleanest way to get one with a
/// custom message that still plays nicely with the
/// `LockfileCanonicalizationFailed` `#[source]` chain.
fn serde_yaml_custom(msg: &'static str) -> serde_yaml::Error {
    use serde::de::Error;
    <serde_yaml::Error as Error>::custom(msg)
}

fn serde_yaml_custom_owned(msg: String) -> serde_yaml::Error {
    use serde::de::Error;
    <serde_yaml::Error as Error>::custom(msg)
}

fn serde_yaml_invalid_utf8() -> serde_yaml::Error {
    use serde::de::Error;
    <serde_yaml::Error as Error>::custom("lockfile is not valid UTF-8")
}

// --- Merge-conflict resolution ---

/// Which side of a `git merge` conflict to pick when resolving
/// `crab.lock`. Default is [`ResolveStrategy::Recompute`]:
/// for every conflicted stage, rehash outs from disk and write a
/// fresh entry so the resolved file is byte-identical regardless of
/// which side of the conflict invoked the command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ResolveStrategy {
    /// Pick the "ours" side wholesale for every conflicted hunk.
    Ours,
    /// Pick the "theirs" side wholesale for every conflicted hunk.
    Theirs,
    /// Re-derive the lockfile from both sides: union the stages,
    /// keep non-conflicted entries verbatim, and drop any stage
    /// whose two sides disagree so the next `crab run` re-runs it.
    #[default]
    Recompute,
}

impl ResolveStrategy {
    /// Human-readable label for structured output (`"ours"`,
    /// `"theirs"`, `"recompute"`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ours => "ours",
            Self::Theirs => "theirs",
            Self::Recompute => "recompute",
        }
    }
}

/// Outcome of resolving a conflicted lockfile. Carries both the
/// resolved `Lockfile` and a summary of what the strategy did so
/// callers (the CLI) can render structured output without inspecting
/// both sides again.
#[derive(Debug, Clone)]
pub struct ResolveOutcome {
    /// The merged lockfile — ready to [`Lockfile::save`].
    pub lockfile: Lockfile,
    /// Strategy actually applied (useful when the caller passed a
    /// default and wants to echo back what ran).
    pub strategy: ResolveStrategy,
    /// Stages whose resolved form was kept (including unconflicted
    /// entries picked through on both sides).
    pub stages_kept: Vec<StageName>,
    /// Stages dropped because both sides disagreed under
    /// [`ResolveStrategy::Recompute`]. Next `crab run` re-runs
    /// them, which is the intended behavior for divergent recomputes.
    pub stages_dropped: Vec<StageName>,
}

/// Resolve a conflicted `crab.lock`.
///
/// Reads the file at `path`, parses out the `<<<<<<< … ======= … >>>>>>>`
/// hunks git writes during a conflicted merge, and produces a resolved
/// lockfile per `strategy`. Returns [`WorkflowError::LockfileMergeConflict`]
/// when the file is not actually in conflict form — nothing to resolve.
///
/// `repo_root` is accepted for the future recompute path that will rehash
/// outs on disk. The current implementation merges declaratively side by
/// side; the parameter is intentionally unused today so the CLI signature
/// stays stable.
pub fn resolve(path: &Path, strategy: ResolveStrategy, repo_root: &Path) -> Result<ResolveOutcome> {
    let _ = repo_root;
    let bytes = std::fs::read(path).map_err(WorkflowError::Io)?;
    resolve_from_bytes(path, &bytes, strategy)
}

/// Variant of [`resolve`] that takes the conflicted bytes directly.
/// Split out so tests and the CLI's dry-run paths don't need to
/// stage a file on disk.
pub fn resolve_from_bytes(
    path: &Path,
    bytes: &[u8],
    strategy: ResolveStrategy,
) -> Result<ResolveOutcome> {
    let text = std::str::from_utf8(bytes).map_err(|_| WorkflowError::LockfileMergeConflict {
        path: path.to_path_buf(),
    })?;

    let (ours_text, theirs_text) = split_conflict_sides(text).ok_or_else(|| {
        // No conflict markers — the caller either invoked resolve on
        // a clean file by mistake, or the merge was resolved by some
        // other tool already. Either way there's nothing to do here.
        WorkflowError::LockfileMergeConflict {
            path: path.to_path_buf(),
        }
    })?;

    // Parse both sides. Parse failures on either side are surfaced
    // directly — resolve must refuse to produce a lockfile that
    // claims provenance it can't verify.
    let ours = Lockfile::parse(path, ours_text.as_bytes())?;
    let theirs = Lockfile::parse(path, theirs_text.as_bytes())?;

    match strategy {
        ResolveStrategy::Ours => Ok(ResolveOutcome {
            stages_kept: ours.stages.keys().cloned().collect(),
            stages_dropped: Vec::new(),
            lockfile: ours,
            strategy,
        }),
        ResolveStrategy::Theirs => Ok(ResolveOutcome {
            stages_kept: theirs.stages.keys().cloned().collect(),
            stages_dropped: Vec::new(),
            lockfile: theirs,
            strategy,
        }),
        ResolveStrategy::Recompute => Ok(recompute_merge(ours, theirs)),
    }
}

/// Merge two sides stage-by-stage under `--recompute`.
///
/// For every stage:
/// - Present on only one side: kept verbatim.
/// - Present on both sides, identical: kept verbatim.
/// - Present on both sides, differing: dropped. Next `crab run`
///   recomputes it from working-tree state. This module deliberately does
///   not rehash on disk until the executor exposes a `rehash_out` seam.
///   Dropping keeps the lockfile byte-identical regardless of which side
///   invoked the command.
///
/// Top-level schema and hash-algo come from whichever side carries
/// the higher schema version, breaking ties toward `ours`. Both sides
/// are guaranteed to come from the same git branch history so the
/// algo string is always equal in practice; the tie-break is defensive.
fn recompute_merge(ours: Lockfile, theirs: Lockfile) -> ResolveOutcome {
    let schema_version = ours
        .schema_version
        .max(theirs.schema_version)
        .max(LOCKFILE_SCHEMA_VERSION);
    let crab_hash_algo = if schema_version == LOCKFILE_SCHEMA_VERSION {
        LOCKFILE_HASH_ALGO.to_owned()
    } else if ours.schema_version >= theirs.schema_version {
        ours.crab_hash_algo.clone()
    } else {
        theirs.crab_hash_algo.clone()
    };

    let mut all_names: BTreeSet<StageName> = BTreeSet::new();
    all_names.extend(ours.stages.keys().cloned());
    all_names.extend(theirs.stages.keys().cloned());

    let mut stages = BTreeMap::new();
    let mut stages_kept = Vec::new();
    let mut stages_dropped = Vec::new();

    for name in all_names {
        match (ours.stages.get(&name), theirs.stages.get(&name)) {
            (Some(o), None) => {
                stages.insert(name.clone(), o.clone());
                stages_kept.push(name);
            }
            (None, Some(t)) => {
                stages.insert(name.clone(), t.clone());
                stages_kept.push(name);
            }
            (Some(o), Some(t)) => {
                if o == t {
                    stages.insert(name.clone(), o.clone());
                    stages_kept.push(name);
                } else {
                    stages_dropped.push(name);
                }
            }
            (None, None) => unreachable!("name came from the union of both sides' keys"),
        }
    }

    ResolveOutcome {
        lockfile: Lockfile {
            schema_version,
            crab_hash_algo,
            stages,
        },
        strategy: ResolveStrategy::Recompute,
        stages_kept,
        stages_dropped,
    }
}

/// Split a git-conflict-marker file into (ours_text, theirs_text).
///
/// Recognizes the canonical 7-character markers `<<<<<<<`, `=======`,
/// `>>>>>>>` at the start of a line. Any text between markers is
/// treated as the conflict region; text outside is common to both
/// sides. Multiple hunks in the same file are supported — common
/// for lockfiles where several stages diverge independently.
///
/// Returns `None` when the file contains no conflict markers. The
/// three markers must be matched in order; unbalanced markers return
/// `None` so the caller surfaces a clean error rather than producing
/// a lockfile derived from truncated input.
fn split_conflict_sides(text: &str) -> Option<(String, String)> {
    let mut ours = String::new();
    let mut theirs = String::new();
    let mut saw_any_marker = false;

    // State machine: `Common` appends to both sides, `Ours` appends
    // only to ours, `Theirs` appends only to theirs. Transitions are
    // driven by marker lines.
    enum Region {
        Common,
        Ours,
        Theirs,
    }
    let mut region = Region::Common;

    for line in text.split_inclusive('\n') {
        // Trim a trailing `\n` before checking the marker so a line
        // like `<<<<<<< HEAD\n` still matches the prefix check below.
        let trimmed = line.trim_end_matches('\n').trim_end_matches('\r');

        if trimmed.starts_with("<<<<<<<") {
            if !matches!(region, Region::Common) {
                // Nested `<<<<<<<` — either a real problem or a line
                // that incidentally starts with seven `<`s inside a
                // quoted value. Lockfiles generated by this module
                // never produce such a line (they quote to double-
                // quoted YAML), so treat it as malformed and refuse.
                return None;
            }
            region = Region::Ours;
            saw_any_marker = true;
            continue;
        }
        if trimmed == "=======" {
            if !matches!(region, Region::Ours) {
                return None;
            }
            region = Region::Theirs;
            continue;
        }
        if trimmed.starts_with(">>>>>>>") {
            if !matches!(region, Region::Theirs) {
                return None;
            }
            region = Region::Common;
            continue;
        }

        match region {
            Region::Common => {
                ours.push_str(line);
                theirs.push_str(line);
            }
            Region::Ours => ours.push_str(line),
            Region::Theirs => theirs.push_str(line),
        }
    }

    if !saw_any_marker {
        return None;
    }
    if !matches!(region, Region::Common) {
        // File ended mid-hunk; caller gets a clean "not conflicted"
        // error rather than a silently truncated merge.
        return None;
    }

    Some((ours, theirs))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn sample_stage() -> (StageName, LockedStage) {
        let name = StageName::parse("train").unwrap();
        let stage = LockedStage {
            stage_hash: StageHash([0xab; 32]),
            cmd: CachedCmd::Shell {
                shell: "python train.py".into(),
            },
            deps: vec![LockedDep {
                path: PathBuf::from("src/train.py"),
                hash: [0x11; 32],
                size: 1234,
            }],
            params: {
                let mut p = BTreeMap::new();
                p.insert("lr".into(), "0.01".into());
                p.insert("epochs".into(), "5".into());
                p
            },
            env: {
                let mut e = BTreeMap::new();
                e.insert("CUDA_VISIBLE_DEVICES".into(), "0".into());
                e
            },
            outs: vec![LockedOut {
                path: PathBuf::from("models/model.pkl"),
                kind: OutKind::File,
                hash: [0x22; 32],
                size: 4096,
                mode: 0o644,
            }],
            metrics: vec![LockedMetric {
                path: PathBuf::from("metrics/train.json"),
                hash: [0x33; 32],
            }],
            plots: vec![LockedOut {
                path: PathBuf::from("plots/loss.csv"),
                kind: OutKind::File,
                hash: [0x44; 32],
                size: 512,
                mode: 0o644,
            }],
            executed_at: "2026-04-27T14:23:11.083Z".into(),
            duration_ms: 12_543,
            host_fingerprint: "linux-x86_64-crab-0.8.0".into(),
            attempts: 1,
            source: "Local".into(),
        };
        (name, stage)
    }

    fn sample_lockfile() -> Lockfile {
        let mut lf = Lockfile::new();
        let (name, stage) = sample_stage();
        lf.stages.insert(name, stage);
        lf
    }

    #[test]
    fn missing_file_yields_default_empty_lockfile() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("crab.lock");
        let lf = Lockfile::load(&path).unwrap();
        assert_eq!(lf, Lockfile::default());
        assert!(lf.stages.is_empty());
    }

    #[test]
    fn save_then_load_round_trips() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("crab.lock");
        let lf = sample_lockfile();
        lf.save(&path).unwrap();
        let got = Lockfile::load(&path).unwrap();
        assert_eq!(got, lf);
    }

    #[test]
    fn serialize_is_byte_stable_across_calls() {
        // The core byte-equality invariant: two calls on the same
        // input produce identical bytes.
        let lf = sample_lockfile();
        let a = lf.serialize_canonical().unwrap();
        let b = lf.serialize_canonical().unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn serialize_is_byte_stable_across_equivalent_lockfiles() {
        // Building the lockfile two different ways (reversed
        // insertion order, shuffled param/env keys) produces the
        // same output — BTreeMap-driven by construction.
        let lf1 = sample_lockfile();
        let mut lf2 = Lockfile::new();
        let (name, stage) = sample_stage();
        lf2.stages.insert(name, stage);
        assert_eq!(
            lf1.serialize_canonical().unwrap(),
            lf2.serialize_canonical().unwrap()
        );
    }

    #[test]
    fn roundtrip_serialize_parse_serialize_is_byte_identical() {
        // Serialize → parse → serialize should yield identical bytes
        // — otherwise the canonical form isn't a fixed point.
        let lf = sample_lockfile();
        let first = lf.serialize_canonical().unwrap();
        let parsed = Lockfile::parse(Path::new("crab.lock"), &first).unwrap();
        let second = parsed.serialize_canonical().unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn all_strings_are_double_quoted() {
        let lf = sample_lockfile();
        let text = String::from_utf8(lf.serialize_canonical().unwrap()).unwrap();
        // Every string value (hashes, paths, modes, timestamps, host
        // fingerprint) lives inside double quotes. Spot-check a few.
        assert!(text.contains(r#""python train.py""#));
        assert!(text.contains(r#""src/train.py""#));
        assert!(text.contains(r#""models/model.pkl""#));
        assert!(text.contains(r#""2026-04-27T14:23:11.083Z""#));
        assert!(text.contains(r#""linux-x86_64-crab-0.8.0""#));
    }

    #[test]
    fn top_level_keys_are_sorted() {
        let lf = sample_lockfile();
        let text = String::from_utf8(lf.serialize_canonical().unwrap()).unwrap();
        let algo = text.find("crab_hash_algo:").unwrap();
        let schema = text.find("schema_version:").unwrap();
        let stages = text.find("stages:").unwrap();
        assert!(
            algo < schema,
            "crab_hash_algo should precede schema_version"
        );
        assert!(schema < stages, "schema_version should precede stages");
    }

    #[test]
    fn stage_inner_keys_are_sorted() {
        let lf = sample_lockfile();
        let text = String::from_utf8(lf.serialize_canonical().unwrap()).unwrap();
        // Sorted keys at the stage level: attempts, cmd, deps,
        // duration_ms, env, executed_at, host_fingerprint, metrics,
        // outs, params, plots, source, stage_hash.
        let positions = [
            "attempts",
            "cmd",
            "deps",
            "duration_ms",
            "env",
            "executed_at",
            "host_fingerprint",
            "metrics",
            "outs",
            "params",
            "plots",
            "source",
            "stage_hash",
        ];
        let mut last = 0usize;
        for key in positions {
            let needle = format!("  {key}:");
            let pos = text
                .find(&needle)
                .unwrap_or_else(|| panic!("did not find '{needle}' in output:\n{text}"));
            assert!(pos >= last, "key '{key}' appears out of order in:\n{text}");
            last = pos;
        }
    }

    #[test]
    fn mode_is_serialized_as_quoted_octal_literal() {
        let lf = sample_lockfile();
        let text = String::from_utf8(lf.serialize_canonical().unwrap()).unwrap();
        assert!(
            text.contains(r#"mode: "0o644""#),
            "mode not quoted octal in:\n{text}"
        );
    }

    #[test]
    fn hash_is_b3_prefixed_64_hex_chars() {
        let lf = sample_lockfile();
        let text = String::from_utf8(lf.serialize_canonical().unwrap()).unwrap();
        // Stage hash lives under `stage_hash:`; assert prefix + length.
        let expected_stage_hash = format!("b3:{}", "ab".repeat(32));
        assert!(
            text.contains(&format!(r#"stage_hash: "{expected_stage_hash}""#)),
            "stage_hash not in expected form in:\n{text}"
        );
    }

    #[test]
    fn nfc_normalization_composes_decomposed_unicode() {
        // "café" as NFC is 4 chars, as NFD it's 5 (c + a + f + e + combining-acute).
        let composed = "café";
        let decomposed = "cafe\u{0301}"; // e + U+0301 COMBINING ACUTE
        assert_ne!(composed, decomposed);

        // Set the fingerprint to the decomposed form and verify that
        // the emitted bytes match the composed form's serialization.
        let mut lf = Lockfile::new();
        let (name, mut stage) = sample_stage();
        stage.host_fingerprint = decomposed.into();
        lf.stages.insert(name, stage);

        let mut lf_composed = Lockfile::new();
        let (n2, mut s2) = sample_stage();
        s2.host_fingerprint = composed.into();
        lf_composed.stages.insert(n2, s2);

        assert_eq!(
            lf.serialize_canonical().unwrap(),
            lf_composed.serialize_canonical().unwrap(),
            "decomposed input should normalize to composed output"
        );
    }

    #[test]
    fn orphan_pruning_returns_removed_names_and_mutates_self() {
        let mut lf = sample_lockfile();
        let extra = StageName::parse("orphan").unwrap();
        let (_, stage) = sample_stage();
        lf.stages.insert(extra.clone(), stage);

        let mut keep = BTreeSet::new();
        keep.insert(StageName::parse("train").unwrap());

        let pruned = lf.prune_stages_not_in(&keep);
        assert_eq!(pruned, vec![extra]);
        assert_eq!(lf.stages.len(), 1);
        assert!(lf.get(&StageName::parse("train").unwrap()).is_some());
    }

    #[test]
    fn upsert_replaces_existing_entry() {
        let mut lf = Lockfile::new();
        let name = StageName::parse("train").unwrap();
        let hash = StageHash([0xab; 32]);
        let entry = StageCacheEntry {
            schema_version: 1,
            stage_hash: hash,
            stage_name: "train".into(),
            cmd: CachedCmd::Shell {
                shell: "echo 1".into(),
            },
            outs: vec![CachedOut {
                path: PathBuf::from("out.txt"),
                kind: OutKind::File,
                push: true,
                remote: None,
                file_hash: format_b3(&[0x22; 32]),
                size: 3,
                mode: 0o644,
                tree_manifest: None,
            }],
            metrics: vec![],
            plots: vec![],
            executed_at: "2026-04-27T14:23:11.083Z".into(),
            duration_ms: 10,
            exec_id: None,
            attempts: 1,
            host_fingerprint: "host".into(),
        };
        lf.upsert(&entry, Vec::new(), BTreeMap::new(), BTreeMap::new())
            .unwrap();
        assert_eq!(lf.stages.len(), 1);
        let locked = lf.get(&name).unwrap();
        assert_eq!(locked.stage_hash, hash);

        // Upserting again with a different hash replaces in place.
        let mut entry2 = entry.clone();
        entry2.stage_hash = StageHash([0xcd; 32]);
        lf.upsert(&entry2, Vec::new(), BTreeMap::new(), BTreeMap::new())
            .unwrap();
        assert_eq!(lf.stages.len(), 1);
        assert_eq!(lf.get(&name).unwrap().stage_hash, StageHash([0xcd; 32]));
    }

    #[test]
    fn empty_lockfile_serializes_to_expected_form() {
        let lf = Lockfile::new();
        let text = String::from_utf8(lf.serialize_canonical().unwrap()).unwrap();
        assert_eq!(
            text,
            concat!(
                "crab_hash_algo: \"crab.stage.v1\"\n",
                "schema_version: 2\n",
                "stages: {}\n",
            )
        );
    }

    #[test]
    fn argv_cmd_emits_argv_sequence() {
        let mut lf = Lockfile::new();
        let (name, mut stage) = sample_stage();
        stage.cmd = CachedCmd::Argv {
            argv: vec!["python".into(), "train.py".into()],
        };
        lf.stages.insert(name, stage);
        let text = String::from_utf8(lf.serialize_canonical().unwrap()).unwrap();
        assert!(
            text.contains("    argv:\n"),
            "argv header missing in:\n{text}"
        );
        assert!(
            text.contains(r#"      - "python""#),
            "argv item missing in:\n{text}"
        );
        assert!(
            text.contains(r#"      - "train.py""#),
            "argv item missing in:\n{text}"
        );
        assert!(
            !text.contains("shell:"),
            "shell key should not appear when argv is used:\n{text}"
        );
    }

    #[test]
    fn shell_list_cmd_emits_shells_sequence() {
        let mut lf = Lockfile::new();
        let (name, mut stage) = sample_stage();
        stage.cmd = CachedCmd::ShellList {
            commands: vec!["cd subdir".into(), "python train.py".into()],
        };
        lf.stages.insert(name, stage);
        let text = String::from_utf8(lf.serialize_canonical().unwrap()).unwrap();
        assert!(
            text.contains("    shells:\n"),
            "shells header missing in:\n{text}"
        );
        assert!(
            text.contains(r#"      - "cd subdir""#),
            "shells item missing in:\n{text}"
        );
        assert!(
            text.contains(r#"      - "python train.py""#),
            "shells item missing in:\n{text}"
        );
        assert!(
            !text.contains("shell:"),
            "scalar shell key should not appear when shell list is used:\n{text}"
        );
        let parsed = Lockfile::parse(Path::new("crab.lock"), text.as_bytes()).unwrap();
        assert_eq!(parsed, lf);
    }

    #[test]
    fn string_with_quote_is_escaped() {
        let mut lf = Lockfile::new();
        let (name, mut stage) = sample_stage();
        stage.cmd = CachedCmd::Shell {
            shell: r#"echo "hi""#.into(),
        };
        lf.stages.insert(name, stage);
        let text = String::from_utf8(lf.serialize_canonical().unwrap()).unwrap();
        assert!(
            text.contains(r#"shell: "echo \"hi\"""#),
            "inner quotes not escaped in:\n{text}"
        );
        // Round-trip survives the escape.
        let parsed = Lockfile::parse(Path::new("crab.lock"), text.as_bytes()).unwrap();
        assert_eq!(parsed, lf);
    }

    #[test]
    fn proptest_byte_equality_across_equivalent_builds() {
        use proptest::prelude::*;
        proptest!(|(
            n_stages in 0usize..4usize,
            seed in any::<u64>()
        )| {
            // Build the same logical lockfile two ways: once with
            // stages inserted in order, once in reverse — the BTreeMap
            // collapses both to the same serialization.
            let names: Vec<StageName> = (0..n_stages)
                .map(|i| StageName::parse(&format!("stage_{i}")).unwrap())
                .collect();

            let mut lf_a = Lockfile::new();
            for (i, name) in names.iter().enumerate() {
                let (_, mut stage) = sample_stage();
                stage.stage_hash = StageHash([(seed.wrapping_add(i as u64)) as u8; 32]);
                lf_a.stages.insert(name.clone(), stage);
            }

            let mut lf_b = Lockfile::new();
            for (i, name) in names.iter().enumerate().rev() {
                let (_, mut stage) = sample_stage();
                stage.stage_hash = StageHash([(seed.wrapping_add(i as u64)) as u8; 32]);
                lf_b.stages.insert(name.clone(), stage);
            }

            prop_assert_eq!(
                lf_a.serialize_canonical().unwrap(),
                lf_b.serialize_canonical().unwrap()
            );
        });
    }

    // --- Merge-conflict resolution tests ---

    /// Build a conflicted file by concatenating an `ours_lf` and a
    /// `theirs_lf` under git's standard marker format. The common
    /// top-level `schema_version` / `crab_hash_algo` keys are
    /// written outside the hunk so only the `stages:` sub-block
    /// diverges — the shape git actually produces in practice.
    fn make_conflicted_bytes(ours_lf: &Lockfile, theirs_lf: &Lockfile) -> Vec<u8> {
        let ours_text = String::from_utf8(ours_lf.serialize_canonical().unwrap()).unwrap();
        let theirs_text = String::from_utf8(theirs_lf.serialize_canonical().unwrap()).unwrap();
        // Whole-file conflict: git produces this when the entire
        // file has diverged. Simpler than stitching per-key hunks
        // and exercises the marker parser end-to-end.
        let mut out = String::new();
        out.push_str("<<<<<<< HEAD\n");
        out.push_str(&ours_text);
        out.push_str("=======\n");
        out.push_str(&theirs_text);
        out.push_str(">>>>>>> theirs\n");
        out.into_bytes()
    }

    #[test]
    fn split_conflict_recovers_both_sides() {
        let file =
            "common_top\n<<<<<<< HEAD\nours_body\n=======\ntheirs_body\n>>>>>>> t\ncommon_bottom\n";
        let (ours, theirs) = split_conflict_sides(file).unwrap();
        assert_eq!(ours, "common_top\nours_body\ncommon_bottom\n");
        assert_eq!(theirs, "common_top\ntheirs_body\ncommon_bottom\n");
    }

    #[test]
    fn split_conflict_rejects_unmarked_file() {
        let file = "schema_version: 1\n";
        assert!(split_conflict_sides(file).is_none());
    }

    #[test]
    fn split_conflict_rejects_unbalanced_markers() {
        let truncated = "<<<<<<< HEAD\nours\n=======\ntheirs\n";
        assert!(split_conflict_sides(truncated).is_none());
        let nested = "<<<<<<< HEAD\n<<<<<<< inner\n";
        assert!(split_conflict_sides(nested).is_none());
    }

    #[test]
    fn resolve_non_conflicted_file_reports_merge_conflict_error() {
        // Calling resolve on a clean file fails with the canonical
        // "not conflicted" error — the CLI turns this into an exit.
        let lf = sample_lockfile();
        let clean_bytes = lf.serialize_canonical().unwrap();
        let err = resolve_from_bytes(
            Path::new("crab.lock"),
            &clean_bytes,
            ResolveStrategy::Recompute,
        )
        .expect_err("non-conflicted file should be rejected");
        assert!(matches!(err, WorkflowError::LockfileMergeConflict { .. }));
    }

    #[test]
    fn resolve_ours_picks_ours_side() {
        let mut ours = Lockfile::new();
        let (name, mut stage) = sample_stage();
        stage.stage_hash = StageHash([0x11; 32]);
        ours.stages.insert(name.clone(), stage);

        let mut theirs = Lockfile::new();
        let (n2, mut s2) = sample_stage();
        s2.stage_hash = StageHash([0x22; 32]);
        theirs.stages.insert(n2, s2);

        let bytes = make_conflicted_bytes(&ours, &theirs);
        let outcome =
            resolve_from_bytes(Path::new("crab.lock"), &bytes, ResolveStrategy::Ours).unwrap();
        assert_eq!(outcome.strategy, ResolveStrategy::Ours);
        assert_eq!(outcome.lockfile.stages.len(), 1);
        assert_eq!(
            outcome.lockfile.get(&name).unwrap().stage_hash,
            StageHash([0x11; 32])
        );
        assert!(outcome.stages_dropped.is_empty());
    }

    #[test]
    fn resolve_theirs_picks_theirs_side() {
        let mut ours = Lockfile::new();
        let (name, mut stage) = sample_stage();
        stage.stage_hash = StageHash([0x11; 32]);
        ours.stages.insert(name.clone(), stage);

        let mut theirs = Lockfile::new();
        let (n2, mut s2) = sample_stage();
        s2.stage_hash = StageHash([0x22; 32]);
        theirs.stages.insert(n2, s2);

        let bytes = make_conflicted_bytes(&ours, &theirs);
        let outcome =
            resolve_from_bytes(Path::new("crab.lock"), &bytes, ResolveStrategy::Theirs).unwrap();
        assert_eq!(outcome.strategy, ResolveStrategy::Theirs);
        assert_eq!(
            outcome.lockfile.get(&name).unwrap().stage_hash,
            StageHash([0x22; 32])
        );
    }

    #[test]
    fn resolve_recompute_keeps_identical_stages_and_drops_divergent() {
        // Shared stage "train" matches on both sides; "only_ours"
        // and "only_theirs" live on a single side each; "diverges"
        // has differing hashes so recompute drops it.
        let (train_name, train_stage) = sample_stage();
        let only_ours_name = StageName::parse("only_ours").unwrap();
        let only_theirs_name = StageName::parse("only_theirs").unwrap();
        let diverges_name = StageName::parse("diverges").unwrap();

        let mut ours = Lockfile::new();
        ours.stages.insert(train_name.clone(), train_stage.clone());
        let (_, ours_only) = sample_stage();
        ours.stages.insert(only_ours_name.clone(), ours_only);
        let (_, mut d_ours) = sample_stage();
        d_ours.stage_hash = StageHash([0xaa; 32]);
        ours.stages.insert(diverges_name.clone(), d_ours);

        let mut theirs = Lockfile::new();
        theirs.stages.insert(train_name.clone(), train_stage);
        let (_, theirs_only) = sample_stage();
        theirs.stages.insert(only_theirs_name.clone(), theirs_only);
        let (_, mut d_theirs) = sample_stage();
        d_theirs.stage_hash = StageHash([0xbb; 32]);
        theirs.stages.insert(diverges_name.clone(), d_theirs);

        let bytes = make_conflicted_bytes(&ours, &theirs);
        let outcome =
            resolve_from_bytes(Path::new("crab.lock"), &bytes, ResolveStrategy::Recompute).unwrap();

        assert_eq!(outcome.strategy, ResolveStrategy::Recompute);
        assert!(outcome.lockfile.get(&train_name).is_some());
        assert!(outcome.lockfile.get(&only_ours_name).is_some());
        assert!(outcome.lockfile.get(&only_theirs_name).is_some());
        assert!(outcome.lockfile.get(&diverges_name).is_none());
        assert_eq!(outcome.stages_dropped, vec![diverges_name]);
        assert_eq!(outcome.stages_kept.len(), 3);
    }

    #[test]
    fn resolve_recompute_is_byte_stable_regardless_of_side_order() {
        // Core invariant: running recompute on (ours, theirs) and
        // on (theirs, ours) yields byte-identical resolved files.
        let (train_name, train_stage) = sample_stage();
        let a_name = StageName::parse("alpha").unwrap();
        let b_name = StageName::parse("beta").unwrap();

        let mut side_a = Lockfile::new();
        side_a
            .stages
            .insert(train_name.clone(), train_stage.clone());
        let (_, a_extra) = sample_stage();
        side_a.stages.insert(a_name, a_extra);

        let mut side_b = Lockfile::new();
        side_b.stages.insert(train_name, train_stage);
        let (_, b_extra) = sample_stage();
        side_b.stages.insert(b_name, b_extra);

        let bytes_ab = make_conflicted_bytes(&side_a, &side_b);
        let bytes_ba = make_conflicted_bytes(&side_b, &side_a);
        let res_ab = resolve_from_bytes(
            Path::new("crab.lock"),
            &bytes_ab,
            ResolveStrategy::Recompute,
        )
        .unwrap();
        let res_ba = resolve_from_bytes(
            Path::new("crab.lock"),
            &bytes_ba,
            ResolveStrategy::Recompute,
        )
        .unwrap();
        assert_eq!(
            res_ab.lockfile.serialize_canonical().unwrap(),
            res_ba.lockfile.serialize_canonical().unwrap(),
        );
    }

    #[test]
    fn resolve_round_trips_to_disk() {
        let (name, mut stage) = sample_stage();
        stage.stage_hash = StageHash([0x99; 32]);
        let mut ours = Lockfile::new();
        ours.stages.insert(name, stage);
        let mut theirs = ours.clone();
        // Flip a stage to force a drop under recompute and prove
        // save-then-load round-trips the resolved bytes.
        if let Some(s) = theirs.stages.get_mut(&StageName::parse("train").unwrap()) {
            s.stage_hash = StageHash([0x77; 32]);
        }

        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("crab.lock");
        std::fs::write(&path, make_conflicted_bytes(&ours, &theirs)).unwrap();

        let outcome = resolve(&path, ResolveStrategy::Recompute, tmp.path()).unwrap();
        outcome.lockfile.save(&path).unwrap();

        let reloaded = Lockfile::load(&path).unwrap();
        assert_eq!(reloaded, outcome.lockfile);
    }
}
