//! Content-addressed stage hashing.
//!
//! The stage hash is a 32-byte Blake3 digest over a canonical
//! serialization of everything that can affect a stage's output:
//! command, resolved dep hashes, params, env, declared outs, and a
//! version-prefix byte. Canonicalization uses sorted keys and length-
//! prefixed framing so that permutations of declared sets produce the
//! same hash and distinct inputs never collide.
//!
//! The format is versioned via the literal prefix `b"crab.stage.v3\n"`.
//! Changing any framing rule requires bumping that prefix, which
//! invalidates every existing stage cache entry — treat as
//! semver-relevant.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::checkpoint::{CHECKPOINT_PROTOCOL_VERSION, CHECKPOINT_SCHEMA_VERSION};
use crate::sandbox::HERMETIC_SANDBOX_POLICY_VERSION;
use crate::stage::{Cmd, EnvSpec, Out, Stage};
use crate::stage_cmd::platform_shell;
use crab_types::workflow::StageHash;

/// A stage with all deps resolved to concrete content hashes plus any
/// params / environment contributing to the hash input.
///
/// `dep_hashes` keys are repo-relative path strings in NFC form; values
/// are the Blake3 content hash of the file (matching the whole-file
/// hash pattern used by `git/clean.rs`). `params` keys are dotted
/// names (e.g. `model.lr`); values are the raw scalar as a UTF-8
/// string so different scalar formats round-trip identically.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolvedStage {
    pub stage: Stage,
    pub dep_hashes: BTreeMap<String, [u8; 32]>,
    pub params: BTreeMap<String, String>,
    pub env: EnvSpec,
    pub cmd: Cmd,
    pub outs: Vec<Out>,
}

/// Version prefix. Bumping this invalidates every existing cache entry.
const V3_PREFIX: &[u8] = b"crab.stage.v3\n";

/// Discriminator bytes placed ahead of each structured section. Fixed
/// values so adding new sections in a future version bump doesn't
/// collide with existing bytes.
mod tag {
    pub const CMD_ARGV: u8 = 0x00;
    pub const CMD_SHELL: u8 = 0xFF;
    pub const CMD_SHELL_LIST: u8 = 0xFE;

    pub const SECT_NAME: u8 = 0x10;
    pub const SECT_CMD: u8 = 0x11;
    pub const SECT_DEPS: u8 = 0x12;
    pub const SECT_PARAMS: u8 = 0x13;
    pub const SECT_ENV: u8 = 0x14;
    pub const SECT_OUTS: u8 = 0x15;
    pub const SECT_FLAGS: u8 = 0x16;
    pub const SECT_WDIR: u8 = 0x17;
    pub const SECT_PLATFORM: u8 = 0x18;

    pub const ENV_INHERIT: u8 = 0x00;
    pub const ENV_ALLOWLIST: u8 = 0x01;
    pub const ENV_EMPTY: u8 = 0x02;
}

/// Compute the Blake3 stage hash for a resolved stage.
pub fn compute(resolved: &ResolvedStage) -> StageHash {
    compute_with_policy_version(resolved, HERMETIC_SANDBOX_POLICY_VERSION)
}

fn compute_with_policy_version(
    resolved: &ResolvedStage,
    hermetic_policy_version: u16,
) -> StageHash {
    let mut h = blake3::Hasher::new();
    h.update(V3_PREFIX);

    push_platform(&mut h);
    push_name(&mut h, resolved.stage.name.as_str());
    push_cmd(&mut h, &resolved.cmd);
    push_deps(&mut h, &resolved.dep_hashes);
    push_params(&mut h, &resolved.params);
    push_env(&mut h, &resolved.env);
    push_outs(&mut h, &resolved.outs);
    push_flags(
        &mut h,
        resolved.stage.nondeterministic,
        resolved.stage.hermetic,
        hermetic_policy_version,
    );
    push_wdir(&mut h, resolved.stage.wdir.as_deref());

    let digest: [u8; 32] = h.finalize().into();
    StageHash(digest)
}

fn push_platform(h: &mut blake3::Hasher) {
    h.update(&[tag::SECT_PLATFORM]);
    push_bytes(h, std::env::consts::OS.as_bytes());
    push_bytes(h, std::env::consts::ARCH.as_bytes());
    push_bytes(h, platform_shell().family.as_bytes());
}

fn push_len(h: &mut blake3::Hasher, n: usize) {
    // Little-endian u64 length prefix keeps framing unambiguous even
    // when two adjacent sections happen to share a byte sequence.
    let len = u64::try_from(n).unwrap_or(u64::MAX);
    h.update(&len.to_le_bytes());
}

fn push_bytes(h: &mut blake3::Hasher, bytes: &[u8]) {
    push_len(h, bytes.len());
    h.update(bytes);
}

fn push_name(h: &mut blake3::Hasher, name: &str) {
    h.update(&[tag::SECT_NAME]);
    push_bytes(h, name.as_bytes());
}

fn push_cmd(h: &mut blake3::Hasher, cmd: &Cmd) {
    h.update(&[tag::SECT_CMD]);
    match cmd {
        Cmd::Argv(args) => {
            h.update(&[tag::CMD_ARGV]);
            push_len(h, args.len());
            for arg in args {
                push_bytes(h, arg.as_bytes());
            }
        }
        Cmd::Shell(s) => {
            h.update(&[tag::CMD_SHELL]);
            push_bytes(h, s.as_bytes());
        }
        Cmd::ShellList(commands) => {
            h.update(&[tag::CMD_SHELL_LIST]);
            push_len(h, commands.len());
            for command in commands {
                push_bytes(h, command.as_bytes());
            }
        }
    }
}

fn push_deps(h: &mut blake3::Hasher, deps: &BTreeMap<String, [u8; 32]>) {
    h.update(&[tag::SECT_DEPS]);
    push_len(h, deps.len());
    // BTreeMap iteration is already sorted — order-independence is
    // baked into the data structure choice.
    for (path, hash) in deps {
        push_bytes(h, path.as_bytes());
        h.update(hash);
    }
}

fn push_params(h: &mut blake3::Hasher, params: &BTreeMap<String, String>) {
    h.update(&[tag::SECT_PARAMS]);
    push_len(h, params.len());
    for (key, value) in params {
        push_bytes(h, key.as_bytes());
        push_bytes(h, value.as_bytes());
    }
}

fn push_env(h: &mut blake3::Hasher, env: &EnvSpec) {
    h.update(&[tag::SECT_ENV]);
    match env {
        EnvSpec::Inherit => {
            h.update(&[tag::ENV_INHERIT]);
        }
        EnvSpec::Allowlist(vars) => {
            h.update(&[tag::ENV_ALLOWLIST]);
            let mut sorted: Vec<&String> = vars.iter().collect();
            sorted.sort();
            sorted.dedup();
            push_len(h, sorted.len());
            for v in sorted {
                push_bytes(h, v.as_bytes());
            }
        }
        EnvSpec::Empty => {
            h.update(&[tag::ENV_EMPTY]);
        }
    }
}

fn push_outs(h: &mut blake3::Hasher, outs: &[Out]) {
    h.update(&[tag::SECT_OUTS]);
    // Outs contribute declaration-level fields only — content hashes
    // flow through the cache entry, not the stage hash. Sort by path
    // string for order independence.
    let mut sorted: Vec<&Out> = outs.iter().collect();
    sorted.sort_by(|a, b| a.path.as_os_str().cmp(b.path.as_os_str()));
    push_len(h, sorted.len());
    for out in sorted {
        let path_bytes = out.path.to_string_lossy().into_owned();
        push_bytes(h, path_bytes.as_bytes());
        push_bytes(h, out.kind.as_str().as_bytes());
        if !out.cache {
            h.update(&[0x01]);
        }
        if !out.push {
            h.update(&[0x02]);
        }
        if out.checkpoint {
            h.update(&[0x03]);
            h.update(&CHECKPOINT_SCHEMA_VERSION.to_le_bytes());
            h.update(&CHECKPOINT_PROTOCOL_VERSION.to_le_bytes());
        }
    }
}

fn push_flags(
    h: &mut blake3::Hasher,
    nondeterministic: bool,
    hermetic: bool,
    hermetic_policy_version: u16,
) {
    h.update(&[tag::SECT_FLAGS]);
    h.update(&[u8::from(nondeterministic), u8::from(hermetic)]);
    if hermetic {
        h.update(&hermetic_policy_version.to_le_bytes());
    }
}

fn push_wdir(h: &mut blake3::Hasher, wdir: Option<&std::path::Path>) {
    h.update(&[tag::SECT_WDIR]);
    match wdir {
        Some(path) => {
            h.update(&[0x01]);
            let path_str = path.to_string_lossy();
            push_bytes(h, path_str.as_bytes());
        }
        None => {
            h.update(&[0x00]);
        }
    }
}

// --- Directory tree hashing ---------------------------------------
//
// Directory deps and outs hash via a canonical tree manifest rather
// than per-file mixing into the stage hash. The manifest is an
// enumeration of every retained entry under the root, in NFC-sorted
// order, serialized with fixed framing and digested with a distinct
// version prefix (`b"crab.tree.v1\n"`). Stability rules mirror the
// stage hash:
//
//   * enumeration excludes `.git/`, `.crab/`, any file whose name
//     contains `.crab.tmp.` (sidecars R11), plus anything matched
//     by the repo's `.gitignore` when `ignore_gitignore` is on.
//   * non-regular, non-directory entries (symlinks, FIFOs, devices,
//     sockets) are rejected — the caller converts the generic error
//     into `StageDepMalformed` / `StageOutMalformed`.
//   * empty directories become zero-entry subtrees (still hashed).
//   * NFC normalization applies to every retained relative path so a
//     working tree that stores a decomposed filename hashes the same
//     as a composed one.
//
// The manifest is returned alongside the hash so the executor can
// persist it later (phase-3 task wires it to a manifest-xorb).

use std::path::{Path, PathBuf};

use globset::{Glob, GlobMatcher};
use unicode_normalization::UnicodeNormalization;

use crate::{Result, WorkflowError as CrabError};

/// Output of [`hash_directory`]. `hash` is the 32-byte Blake3 digest
/// of the canonical manifest; `manifest` is the (path-sorted) entries
/// the hash was computed over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectoryHash {
    pub hash: [u8; 32],
    pub manifest: Vec<TreeEntry>,
}

/// One entry in the directory manifest.
///
/// `path` is repo-relative to the hashed root, NFC-normalized. `mode`
/// captures the unix permission + special bits (masked to `0o7777`);
/// on non-unix platforms it falls back to a stable placeholder so the
/// manifest still round-trips deterministically across builds of the
/// same OS.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeEntry {
    pub path: PathBuf,
    pub kind: TreeEntryKind,
    pub file_hash: [u8; 32],
    pub size: u64,
    pub mode: u32,
}

/// Kind discriminator for [`TreeEntry`]. Matches the file/directory
/// dichotomy of `OutKind` — the tree hasher rejects every other kind
/// at enumeration time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TreeEntryKind {
    File,
    Directory,
}

impl TreeEntryKind {
    fn tag(self) -> u8 {
        match self {
            Self::File => 0x01,
            Self::Directory => 0x02,
        }
    }
}

const V1_TREE_PREFIX: &[u8] = b"crab.tree.v1\n";

/// `.crab.tmp.` appears in the sidecar naming scheme (R11).
/// Matched as a substring of the file name, not a suffix, so
/// users can't accidentally slip one into a dir by renaming.
const SIDECAR_MARKER: &str = ".crab.tmp.";

/// Hash the contents of `root` into a canonical tree manifest + digest.
///
/// `ignore_gitignore` asks the hasher to consult the `.gitignore`
/// rooted at `root` and drop matching entries. Rules that the
/// directory itself lives under a `.gitignore` above `root` are *not*
/// consulted — the hasher is a local read. This mirrors how DVC and
/// Make traditionally treat directory dependencies.
///
/// Returns [`CrabError::StageDepMalformed`] / [`StageOutMalformed`]
/// on non-regular, non-directory entries is the *caller's*
/// responsibility — this function returns [`CrabError::Io`] with a
/// `InvalidInput` kind for those so the caller can convert it to the
/// variant that carries stage / path context. Plain I/O problems
/// surface as [`CrabError::Io`].
pub fn hash_directory(root: &Path, ignore_gitignore: bool) -> Result<DirectoryHash> {
    let root_meta = std::fs::symlink_metadata(root).map_err(CrabError::Io)?;
    if !root_meta.file_type().is_dir() {
        return Err(CrabError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "tree hash root must be a directory",
        )));
    }

    let gitignore = if ignore_gitignore {
        load_gitignore_matchers(root)?
    } else {
        Vec::new()
    };

    let mut entries = Vec::new();
    walk(root, Path::new(""), &gitignore, &mut entries)?;

    // NFC-sort by the normalized path bytes so decomposed /
    // composed forms of the same filename land next to each other
    // deterministically.
    entries.sort_by(|a, b| {
        let a_bytes = path_to_nfc_string(&a.path).into_bytes();
        let b_bytes = path_to_nfc_string(&b.path).into_bytes();
        a_bytes.cmp(&b_bytes)
    });

    let digest = hash_tree_entries(&entries);

    Ok(DirectoryHash {
        hash: digest,
        manifest: entries,
    })
}

/// Hash an already-enumerated tree manifest using Crab's canonical tree format.
#[must_use]
pub fn hash_tree_entries(entries: &[TreeEntry]) -> [u8; 32] {
    let mut sorted = entries.to_vec();
    sorted.sort_by(|a, b| {
        let a_bytes = path_to_nfc_string(&a.path).into_bytes();
        let b_bytes = path_to_nfc_string(&b.path).into_bytes();
        a_bytes.cmp(&b_bytes)
    });

    let mut h = blake3::Hasher::new();
    h.update(V1_TREE_PREFIX);
    push_len(&mut h, sorted.len());
    for e in &sorted {
        let path_bytes = path_to_nfc_string(&e.path).into_bytes();
        push_bytes(&mut h, &path_bytes);
        h.update(&[0x00]);
        h.update(&[e.kind.tag()]);
        h.update(&[0x00]);
        h.update(&e.file_hash);
        h.update(&e.size.to_be_bytes());
        h.update(&e.mode.to_be_bytes());
    }
    h.finalize().into()
}

/// Recursive enumerator. `rel_prefix` is the path of `dir` relative to
/// the original root — empty when called on the root itself, then
/// extended with each directory component as we descend.
fn walk(
    dir: &Path,
    rel_prefix: &Path,
    gitignore: &[GitignoreRule],
    out: &mut Vec<TreeEntry>,
) -> Result<()> {
    // Collect entries first, then sort — sorting the raw
    // directory listing keeps enumeration deterministic even when
    // the filesystem's native order differs across platforms.
    let mut children: Vec<(std::ffi::OsString, std::fs::FileType, std::fs::Metadata)> = Vec::new();
    let reader = std::fs::read_dir(dir).map_err(CrabError::Io)?;
    for entry in reader {
        let entry = entry.map_err(CrabError::Io)?;
        let file_name = entry.file_name();
        let meta = entry.metadata().map_err(CrabError::Io)?;
        let ft = meta.file_type();
        children.push((file_name, ft, meta));
    }
    children.sort_by(|a, b| a.0.cmp(&b.0));

    for (name, ft, meta) in children {
        let name_str = name.to_string_lossy().into_owned();

        // Top-level `.git` and `.crab` directories are hard-
        // excluded regardless of `.gitignore` state — the workflow
        // layer depends on these being invisible to content
        // hashing.
        if rel_prefix.as_os_str().is_empty() && (name_str == ".git" || name_str == ".crab") {
            continue;
        }

        // Sidecar tempfiles (R11) never contribute to a content
        // hash — they can appear at any depth, so check every
        // entry's basename.
        if name_str.contains(SIDECAR_MARKER) {
            continue;
        }

        let rel = if rel_prefix.as_os_str().is_empty() {
            PathBuf::from(&name)
        } else {
            rel_prefix.join(&name)
        };

        if !gitignore.is_empty() && gitignore_matches(gitignore, &rel, ft.is_dir()) {
            continue;
        }

        // Reject non-regular, non-directory entries. The stdlib
        // `file_type` groups FIFOs / sockets / devices under the
        // `!is_dir() && !is_file() && !is_symlink()` combinator on
        // unix; we use that to keep the check portable.
        if ft.is_symlink() {
            return Err(CrabError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("symlink entry at {}", rel.display()),
            )));
        }
        if !ft.is_dir() && !ft.is_file() {
            return Err(CrabError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("non-regular entry at {}", rel.display()),
            )));
        }

        if ft.is_dir() {
            let abs = dir.join(&name);
            // Record the directory itself as a zero-hash marker so
            // empty dirs and mid-tree renames affect the manifest.
            out.push(TreeEntry {
                path: rel.clone(),
                kind: TreeEntryKind::Directory,
                file_hash: [0; 32],
                size: 0,
                mode: entry_mode(&meta),
            });
            walk(&abs, &rel, gitignore, out)?;
        } else {
            let abs = dir.join(&name);
            let hash = hash_file_contents(&abs)?;
            out.push(TreeEntry {
                path: rel,
                kind: TreeEntryKind::File,
                file_hash: hash,
                size: meta.len(),
                mode: entry_mode(&meta),
            });
        }
    }
    Ok(())
}

fn hash_file_contents(path: &Path) -> Result<[u8; 32]> {
    let mut hasher = blake3::Hasher::new();
    let mut file = std::fs::File::open(path).map_err(CrabError::Io)?;
    std::io::copy(&mut file, &mut hasher).map_err(CrabError::Io)?;
    Ok(*hasher.finalize().as_bytes())
}

#[cfg(unix)]
fn entry_mode(meta: &std::fs::Metadata) -> u32 {
    use std::os::unix::fs::MetadataExt;
    meta.mode() & 0o7777
}

#[cfg(not(unix))]
fn entry_mode(_meta: &std::fs::Metadata) -> u32 {
    // Windows builds have no permission bits to compare — stamp a
    // stable placeholder so the manifest still round-trips
    // deterministically on a single platform.
    0o644
}

fn path_to_nfc_string(p: &Path) -> String {
    // Use forward-slashes as the canonical separator — matches the
    // `.gitignore` matcher's view and decouples the manifest from
    // the running platform's native path separator.
    let mut s = String::new();
    let mut first = true;
    for comp in p.components() {
        if !first {
            s.push('/');
        }
        first = false;
        s.push_str(&comp.as_os_str().to_string_lossy());
    }
    s.nfc().collect()
}

/// Compiled `.gitignore` rule. `matcher` is the glob; `negation`
/// indicates a leading `!` (turns the rule into an allowlist entry);
/// `dir_only` indicates a trailing `/` (rule matches directories only).
struct GitignoreRule {
    matcher: GlobMatcher,
    negation: bool,
    dir_only: bool,
}

/// Load `.gitignore` at `root`, compile each pattern into a
/// [`GitignoreRule`], drop blank lines and comments. Returns an empty
/// vector if `.gitignore` doesn't exist.
///
/// This is a best-effort subset of git's own ignore engine: it honors
/// negation (`!`), dir-only (trailing `/`), and anchored patterns
/// (leading `/`). It doesn't consult parent directories or the user's
/// `core.excludesFile`. That's enough to let users opt out of noisy
/// files without us reinventing gitoxide's ignore walker.
fn load_gitignore_matchers(root: &Path) -> Result<Vec<GitignoreRule>> {
    let path = root.join(".gitignore");
    let contents = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(CrabError::Io(e)),
    };
    let mut rules = Vec::new();
    for raw in contents.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (negation, rest) = if let Some(tail) = line.strip_prefix('!') {
            (true, tail)
        } else {
            (false, line)
        };
        let (dir_only, pattern) = if let Some(stem) = rest.strip_suffix('/') {
            (true, stem)
        } else {
            (false, rest)
        };
        let anchored = pattern.starts_with('/');
        let pattern = pattern.trim_start_matches('/');
        // Anchored patterns match from the root only; un-anchored
        // patterns should match at any depth, so prepend `**/` when
        // the user didn't anchor.
        let effective = if anchored || pattern.contains('/') {
            pattern.to_owned()
        } else {
            format!("**/{pattern}")
        };
        let Ok(glob) = Glob::new(&effective) else {
            // Malformed patterns shouldn't break hashing — a user's
            // `.gitignore` has value beyond our use case. Skip.
            continue;
        };
        rules.push(GitignoreRule {
            matcher: glob.compile_matcher(),
            negation,
            dir_only,
        });
    }
    Ok(rules)
}

/// Apply the rules in declaration order, last-match-wins.
fn gitignore_matches(rules: &[GitignoreRule], rel: &Path, is_dir: bool) -> bool {
    let as_slash = path_to_nfc_string(rel);
    let mut ignored = false;
    for rule in rules {
        if rule.dir_only && !is_dir {
            continue;
        }
        if rule.matcher.is_match(as_slash.as_str()) {
            ignored = !rule.negation;
        }
    }
    ignored
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stage::{OutKind, StageName};
    use std::path::PathBuf;

    fn make_stage() -> Stage {
        Stage::new(
            StageName::parse("train").unwrap(),
            Cmd::Shell("python train.py".into()),
        )
    }

    fn resolved(
        cmd: Cmd,
        deps: BTreeMap<String, [u8; 32]>,
        params: BTreeMap<String, String>,
        env: EnvSpec,
        outs: Vec<Out>,
    ) -> ResolvedStage {
        ResolvedStage {
            stage: make_stage(),
            dep_hashes: deps,
            params,
            env,
            cmd,
            outs,
        }
    }

    fn hash_of(r: &ResolvedStage) -> StageHash {
        compute(r)
    }

    fn hash_of_with_policy_version(r: &ResolvedStage, version: u16) -> StageHash {
        compute_with_policy_version(r, version)
    }

    #[test]
    fn hex_is_64_lowercase_chars() {
        let h = StageHash([0xab; 32]);
        let hex = h.as_hex();
        assert_eq!(hex.len(), 64);
        assert!(
            hex.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        );
        assert_eq!(&hex[..4], "abab");
    }

    #[test]
    fn argv_and_shell_with_same_tokens_hash_differently() {
        // R1: Cmd::Argv(["python", "train.py"]) and
        // Cmd::Shell("python train.py") must NOT collide.
        let argv = resolved(
            Cmd::Argv(vec!["python".into(), "train.py".into()]),
            BTreeMap::new(),
            BTreeMap::new(),
            EnvSpec::Empty,
            vec![],
        );
        let shell = resolved(
            Cmd::Shell("python train.py".into()),
            BTreeMap::new(),
            BTreeMap::new(),
            EnvSpec::Empty,
            vec![],
        );
        assert_ne!(hash_of(&argv), hash_of(&shell));
    }

    #[test]
    fn shell_list_and_joined_shell_hash_differently() {
        let shell_list = resolved(
            Cmd::ShellList(vec!["cd subdir".into(), "pwd".into()]),
            BTreeMap::new(),
            BTreeMap::new(),
            EnvSpec::Empty,
            vec![],
        );
        let joined = resolved(
            Cmd::Shell("cd subdir && pwd".into()),
            BTreeMap::new(),
            BTreeMap::new(),
            EnvSpec::Empty,
            vec![],
        );
        assert_ne!(hash_of(&shell_list), hash_of(&joined));
    }

    #[test]
    fn dep_order_does_not_affect_hash() {
        let mut deps_a = BTreeMap::new();
        deps_a.insert("a.txt".into(), [1; 32]);
        deps_a.insert("b.txt".into(), [2; 32]);

        // BTreeMap is always sorted, but the test documents the
        // invariant: insertion order is irrelevant.
        let mut deps_b = BTreeMap::new();
        deps_b.insert("b.txt".into(), [2; 32]);
        deps_b.insert("a.txt".into(), [1; 32]);

        let r_a = resolved(
            Cmd::Shell("cmd".into()),
            deps_a,
            BTreeMap::new(),
            EnvSpec::Empty,
            vec![],
        );
        let r_b = resolved(
            Cmd::Shell("cmd".into()),
            deps_b,
            BTreeMap::new(),
            EnvSpec::Empty,
            vec![],
        );
        assert_eq!(hash_of(&r_a), hash_of(&r_b));
    }

    #[test]
    fn param_order_does_not_affect_hash() {
        let mut params_a = BTreeMap::new();
        params_a.insert("lr".into(), "0.01".into());
        params_a.insert("epochs".into(), "5".into());

        let mut params_b = BTreeMap::new();
        params_b.insert("epochs".into(), "5".into());
        params_b.insert("lr".into(), "0.01".into());

        let r_a = resolved(
            Cmd::Shell("cmd".into()),
            BTreeMap::new(),
            params_a,
            EnvSpec::Empty,
            vec![],
        );
        let r_b = resolved(
            Cmd::Shell("cmd".into()),
            BTreeMap::new(),
            params_b,
            EnvSpec::Empty,
            vec![],
        );
        assert_eq!(hash_of(&r_a), hash_of(&r_b));
    }

    #[test]
    fn env_allowlist_order_does_not_affect_hash() {
        let r_a = resolved(
            Cmd::Shell("cmd".into()),
            BTreeMap::new(),
            BTreeMap::new(),
            EnvSpec::Allowlist(vec!["A".into(), "B".into(), "C".into()]),
            vec![],
        );
        let r_b = resolved(
            Cmd::Shell("cmd".into()),
            BTreeMap::new(),
            BTreeMap::new(),
            EnvSpec::Allowlist(vec!["C".into(), "A".into(), "B".into()]),
            vec![],
        );
        assert_eq!(hash_of(&r_a), hash_of(&r_b));
    }

    #[test]
    fn env_policy_changes_affect_hash() {
        let inherit = resolved(
            Cmd::Shell("cmd".into()),
            BTreeMap::new(),
            BTreeMap::new(),
            EnvSpec::Inherit,
            vec![],
        );
        let empty = resolved(
            Cmd::Shell("cmd".into()),
            BTreeMap::new(),
            BTreeMap::new(),
            EnvSpec::Empty,
            vec![],
        );
        let allow = resolved(
            Cmd::Shell("cmd".into()),
            BTreeMap::new(),
            BTreeMap::new(),
            EnvSpec::Allowlist(vec!["A".into()]),
            vec![],
        );
        assert_ne!(hash_of(&inherit), hash_of(&empty));
        assert_ne!(hash_of(&inherit), hash_of(&allow));
        assert_ne!(hash_of(&empty), hash_of(&allow));
    }

    #[test]
    fn out_order_does_not_affect_hash() {
        let out_a = Out::new(PathBuf::from("a.txt"), OutKind::File);
        let out_b = Out::new(PathBuf::from("b.txt"), OutKind::File);

        let r_a = resolved(
            Cmd::Shell("cmd".into()),
            BTreeMap::new(),
            BTreeMap::new(),
            EnvSpec::Empty,
            vec![out_a.clone(), out_b.clone()],
        );
        let r_b = resolved(
            Cmd::Shell("cmd".into()),
            BTreeMap::new(),
            BTreeMap::new(),
            EnvSpec::Empty,
            vec![out_b, out_a],
        );
        assert_eq!(hash_of(&r_a), hash_of(&r_b));
    }

    #[test]
    fn output_remote_name_does_not_affect_hash() {
        let mut remote = Out::new(PathBuf::from("model.pkl"), OutKind::File);
        remote.remote = Some("cold-storage".to_owned());
        let plain = Out::new(PathBuf::from("model.pkl"), OutKind::File);

        let r_remote = resolved(
            Cmd::Shell("cmd".into()),
            BTreeMap::new(),
            BTreeMap::new(),
            EnvSpec::Empty,
            vec![remote],
        );
        let r_plain = resolved(
            Cmd::Shell("cmd".into()),
            BTreeMap::new(),
            BTreeMap::new(),
            EnvSpec::Empty,
            vec![plain],
        );
        assert_eq!(hash_of(&r_remote), hash_of(&r_plain));
    }

    #[test]
    fn file_vs_directory_out_affects_hash() {
        let file = Out::new(PathBuf::from("out"), OutKind::File);
        let dir = Out::new(PathBuf::from("out"), OutKind::Directory);
        let r_file = resolved(
            Cmd::Shell("cmd".into()),
            BTreeMap::new(),
            BTreeMap::new(),
            EnvSpec::Empty,
            vec![file],
        );
        let r_dir = resolved(
            Cmd::Shell("cmd".into()),
            BTreeMap::new(),
            BTreeMap::new(),
            EnvSpec::Empty,
            vec![dir],
        );
        assert_ne!(hash_of(&r_file), hash_of(&r_dir));
    }

    #[test]
    fn out_cache_policy_affects_hash() {
        let cached = Out::new(PathBuf::from("out"), OutKind::File);
        let mut uncached = cached.clone();
        uncached.cache = false;
        let r_cached = resolved(
            Cmd::Shell("cmd".into()),
            BTreeMap::new(),
            BTreeMap::new(),
            EnvSpec::Empty,
            vec![cached],
        );
        let r_uncached = resolved(
            Cmd::Shell("cmd".into()),
            BTreeMap::new(),
            BTreeMap::new(),
            EnvSpec::Empty,
            vec![uncached],
        );
        assert_ne!(hash_of(&r_cached), hash_of(&r_uncached));
    }

    #[test]
    fn out_push_policy_affects_hash() {
        let pushable = Out::new(PathBuf::from("out"), OutKind::File);
        let mut local_only = pushable.clone();
        local_only.push = false;
        let r_pushable = resolved(
            Cmd::Shell("cmd".into()),
            BTreeMap::new(),
            BTreeMap::new(),
            EnvSpec::Empty,
            vec![pushable],
        );
        let r_local_only = resolved(
            Cmd::Shell("cmd".into()),
            BTreeMap::new(),
            BTreeMap::new(),
            EnvSpec::Empty,
            vec![local_only],
        );
        assert_ne!(hash_of(&r_pushable), hash_of(&r_local_only));
    }

    #[test]
    fn different_commands_hash_differently() {
        let r_a = resolved(
            Cmd::Shell("a".into()),
            BTreeMap::new(),
            BTreeMap::new(),
            EnvSpec::Empty,
            vec![],
        );
        let r_b = resolved(
            Cmd::Shell("b".into()),
            BTreeMap::new(),
            BTreeMap::new(),
            EnvSpec::Empty,
            vec![],
        );
        assert_ne!(hash_of(&r_a), hash_of(&r_b));
    }

    /// Helper: build a resolved stage with the given flags, otherwise
    /// identical. Keeps the flag-sensitivity tests free of boilerplate.
    fn resolved_with_flags(nondeterministic: bool, hermetic: bool) -> ResolvedStage {
        let mut stage = make_stage();
        stage.nondeterministic = nondeterministic;
        stage.hermetic = hermetic;
        ResolvedStage {
            stage,
            dep_hashes: BTreeMap::new(),
            params: BTreeMap::new(),
            env: EnvSpec::Empty,
            cmd: Cmd::Shell("cmd".into()),
            outs: vec![],
        }
    }

    #[test]
    fn hermetic_flag_participates_in_hash() {
        let off = resolved_with_flags(false, false);
        let on = resolved_with_flags(false, true);
        assert_ne!(hash_of(&off), hash_of(&on));
    }

    #[test]
    fn hermetic_policy_version_participates_in_hash() {
        let hermetic = resolved_with_flags(false, true);
        assert_ne!(
            hash_of_with_policy_version(&hermetic, 1),
            hash_of_with_policy_version(&hermetic, 2)
        );

        let non_hermetic = resolved_with_flags(false, false);
        assert_eq!(
            hash_of_with_policy_version(&non_hermetic, 1),
            hash_of_with_policy_version(&non_hermetic, 2)
        );
    }

    #[test]
    fn nondeterministic_flag_participates_in_hash() {
        let off = resolved_with_flags(false, false);
        let on = resolved_with_flags(true, false);
        assert_ne!(hash_of(&off), hash_of(&on));
    }

    #[test]
    fn flag_combinations_all_distinct() {
        let ff = hash_of(&resolved_with_flags(false, false));
        let ft = hash_of(&resolved_with_flags(false, true));
        let tf = hash_of(&resolved_with_flags(true, false));
        let tt = hash_of(&resolved_with_flags(true, true));
        // Four flag combinations must map to four distinct hashes —
        // otherwise a user flipping hermetic or nondeterministic could
        // silently collide with a cached entry.
        let hashes = [ff, ft, tf, tt];
        for i in 0..hashes.len() {
            for j in (i + 1)..hashes.len() {
                assert_ne!(hashes[i], hashes[j], "flag combo collision i={i} j={j}");
            }
        }
    }

    #[test]
    fn default_flags_unchanged_hash() {
        // Belt-and-suspenders: the default `Stage::new` leaves both
        // flags at `false`, which is what the hasher used to hard-code.
        // If this test drifts, every existing cache entry will miss.
        let default = resolved_with_flags(false, false);
        let control = resolved(
            Cmd::Shell("cmd".into()),
            BTreeMap::new(),
            BTreeMap::new(),
            EnvSpec::Empty,
            vec![],
        );
        assert_eq!(hash_of(&default), hash_of(&control));
    }

    #[test]
    fn hash_is_deterministic() {
        let r = resolved(
            Cmd::Argv(vec!["cp".into(), "a".into(), "b".into()]),
            {
                let mut m = BTreeMap::new();
                m.insert("a".into(), [7; 32]);
                m
            },
            {
                let mut m = BTreeMap::new();
                m.insert("k".into(), "v".into());
                m
            },
            EnvSpec::Allowlist(vec!["PATH".into()]),
            vec![Out::new(PathBuf::from("b"), OutKind::File)],
        );
        assert_eq!(hash_of(&r), hash_of(&r));
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(16))]

        /// Property: flipping `hermetic` while holding everything else
        /// constant always produces a distinct stage hash. A collision
        /// here would let a cached non-hermetic result satisfy a
        /// hermetic-declared stage, defeating the sandbox guarantee.
        #[test]
        fn prop_hermetic_flag_changes_hash(nondet: bool, cmd_is_argv: bool) {
            let cmd = if cmd_is_argv {
                Cmd::Argv(vec!["cmd".into()])
            } else {
                Cmd::Shell("cmd".into())
            };
            let mut stage_off = make_stage();
            stage_off.nondeterministic = nondet;
            stage_off.hermetic = false;
            let mut stage_on = stage_off.clone();
            stage_on.hermetic = true;

            let off = ResolvedStage {
                stage: stage_off,
                dep_hashes: BTreeMap::new(),
                params: BTreeMap::new(),
                env: EnvSpec::Empty,
                cmd: cmd.clone(),
                outs: vec![],
            };
            let on = ResolvedStage {
                stage: stage_on,
                dep_hashes: BTreeMap::new(),
                params: BTreeMap::new(),
                env: EnvSpec::Empty,
                cmd,
                outs: vec![],
            };
            proptest::prop_assert_ne!(compute(&off), compute(&on));
        }

        /// Property: flipping `nondeterministic` with everything else
        /// constant always changes the stage hash. Guards against the
        /// `ResolvedStage` codepath forgetting to feed the flag back
        /// into the blake3 input.
        #[test]
        fn prop_nondeterministic_flag_changes_hash(herm: bool, cmd_is_argv: bool) {
            let cmd = if cmd_is_argv {
                Cmd::Argv(vec!["cmd".into()])
            } else {
                Cmd::Shell("cmd".into())
            };
            let mut stage_off = make_stage();
            stage_off.hermetic = herm;
            stage_off.nondeterministic = false;
            let mut stage_on = stage_off.clone();
            stage_on.nondeterministic = true;

            let off = ResolvedStage {
                stage: stage_off,
                dep_hashes: BTreeMap::new(),
                params: BTreeMap::new(),
                env: EnvSpec::Empty,
                cmd: cmd.clone(),
                outs: vec![],
            };
            let on = ResolvedStage {
                stage: stage_on,
                dep_hashes: BTreeMap::new(),
                params: BTreeMap::new(),
                env: EnvSpec::Empty,
                cmd,
                outs: vec![],
            };
            proptest::prop_assert_ne!(compute(&off), compute(&on));
        }
    }

    // --- Directory tree hasher tests ------------------------------

    use tempfile::TempDir;

    fn write_file(root: &Path, rel: &str, bytes: &[u8]) {
        let full = root.join(rel);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(full, bytes).unwrap();
    }

    #[cfg(unix)]
    fn set_mode(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    #[test]
    fn empty_directory_hashes_to_stable_value() {
        let a = TempDir::new().unwrap();
        let b = TempDir::new().unwrap();
        let ha = hash_directory(a.path(), false).unwrap();
        let hb = hash_directory(b.path(), false).unwrap();
        assert_eq!(ha.hash, hb.hash);
        assert!(ha.manifest.is_empty());
    }

    #[test]
    fn non_directory_root_is_rejected() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("f");
        std::fs::write(&file, b"hello").unwrap();
        let err = hash_directory(&file, false).unwrap_err();
        assert!(matches!(err, CrabError::Io(_)));
    }

    #[test]
    fn hash_is_stable_across_repeated_runs() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "a.txt", b"hello");
        write_file(tmp.path(), "sub/b.txt", b"world");

        let h1 = hash_directory(tmp.path(), false).unwrap();
        let h2 = hash_directory(tmp.path(), false).unwrap();
        assert_eq!(h1.hash, h2.hash);
        assert_eq!(h1.manifest, h2.manifest);
    }

    #[test]
    fn changing_file_content_changes_hash() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "a.txt", b"before");
        let first = hash_directory(tmp.path(), false).unwrap().hash;

        write_file(tmp.path(), "a.txt", b"after");
        let second = hash_directory(tmp.path(), false).unwrap().hash;

        assert_ne!(first, second);
    }

    #[test]
    fn nested_dirs_produce_sorted_deterministic_manifest() {
        let tmp = TempDir::new().unwrap();
        // Create files in declared-order chaos so any reliance on
        // enumeration order shows up as a diff against the sorted
        // expectation.
        write_file(tmp.path(), "zeta/c.txt", b"3");
        write_file(tmp.path(), "alpha/a.txt", b"1");
        write_file(tmp.path(), "alpha/b.txt", b"2");

        let result = hash_directory(tmp.path(), false).unwrap();

        let paths: Vec<String> = result
            .manifest
            .iter()
            .map(|e| path_to_nfc_string(&e.path))
            .collect();
        let mut expected = paths.clone();
        expected.sort();
        assert_eq!(paths, expected);

        let dirs: Vec<_> = result
            .manifest
            .iter()
            .filter(|e| e.kind == TreeEntryKind::Directory)
            .map(|e| path_to_nfc_string(&e.path))
            .collect();
        assert!(dirs.contains(&"alpha".to_owned()));
        assert!(dirs.contains(&"zeta".to_owned()));
    }

    #[cfg(unix)]
    #[test]
    fn symlink_inside_directory_is_rejected() {
        use std::os::unix::fs::symlink;

        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "real.txt", b"x");
        symlink(tmp.path().join("real.txt"), tmp.path().join("link.txt")).unwrap();

        let err = hash_directory(tmp.path(), false).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("symlink"), "unexpected error: {msg}");
    }

    #[test]
    fn gitignore_excludes_matched_file() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "keep.txt", b"keep");
        write_file(tmp.path(), "ignored.log", b"drop");
        write_file(tmp.path(), ".gitignore", b"*.log\n");

        let with_ignore = hash_directory(tmp.path(), true).unwrap();
        let names: Vec<String> = with_ignore
            .manifest
            .iter()
            .map(|e| path_to_nfc_string(&e.path))
            .collect();
        assert!(names.iter().any(|n| n == "keep.txt"));
        assert!(
            !names.iter().any(|n| n == "ignored.log"),
            "ignored.log should be excluded, manifest: {names:?}"
        );

        let no_ignore = hash_directory(tmp.path(), false).unwrap();
        assert_ne!(no_ignore.hash, with_ignore.hash);
    }

    #[test]
    fn sidecar_tmp_files_are_excluded() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "out.txt", b"final");
        write_file(tmp.path(), "out.txt.crab.tmp.abc123", b"partial");

        let result = hash_directory(tmp.path(), false).unwrap();
        let names: Vec<String> = result
            .manifest
            .iter()
            .map(|e| path_to_nfc_string(&e.path))
            .collect();
        assert!(names.iter().any(|n| n == "out.txt"));
        assert!(
            !names.iter().any(|n| n.contains(".crab.tmp.")),
            "sidecar must be excluded: {names:?}"
        );
    }

    #[test]
    fn dotgit_and_dotcrab_are_excluded() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), ".git/HEAD", b"ref");
        write_file(tmp.path(), ".crab/staging/x", b"stage");
        write_file(tmp.path(), "visible.txt", b"ok");

        let result = hash_directory(tmp.path(), false).unwrap();
        let names: Vec<String> = result
            .manifest
            .iter()
            .map(|e| path_to_nfc_string(&e.path))
            .collect();
        assert!(names.iter().any(|n| n == "visible.txt"));
        assert!(!names.iter().any(|n| n.starts_with(".git")));
        assert!(!names.iter().any(|n| n.starts_with(".crab")));
    }

    #[cfg(unix)]
    #[test]
    fn different_file_modes_produce_different_hashes() {
        let a = TempDir::new().unwrap();
        let b = TempDir::new().unwrap();
        write_file(a.path(), "x.sh", b"#!/bin/sh\n");
        write_file(b.path(), "x.sh", b"#!/bin/sh\n");
        set_mode(&a.path().join("x.sh"), 0o644);
        set_mode(&b.path().join("x.sh"), 0o755);

        let ha = hash_directory(a.path(), false).unwrap();
        let hb = hash_directory(b.path(), false).unwrap();
        assert_ne!(ha.hash, hb.hash);
    }

    #[test]
    fn nfc_and_nfd_filenames_hash_identically() {
        // "café" — composed (NFC) vs decomposed (NFD). The manifest's
        // NFC normalization should canonicalize both forms before
        // hashing, so the two temp dirs produce the same digest even
        // when one filesystem stores NFC and another stores NFD.
        let nfc_name = "caf\u{00e9}.txt";
        let nfd_name = "cafe\u{0301}.txt";

        let a = TempDir::new().unwrap();
        let b = TempDir::new().unwrap();
        write_file(a.path(), nfc_name, b"content");
        write_file(b.path(), nfd_name, b"content");

        let ha = hash_directory(a.path(), false).unwrap();
        let hb = hash_directory(b.path(), false).unwrap();
        assert_eq!(ha.hash, hb.hash);
    }

    #[test]
    fn empty_subdirectory_is_preserved() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("emptydir")).unwrap();
        let result = hash_directory(tmp.path(), false).unwrap();
        assert_eq!(result.manifest.len(), 1);
        assert_eq!(result.manifest[0].kind, TreeEntryKind::Directory);
        assert_eq!(
            path_to_nfc_string(&result.manifest[0].path),
            "emptydir".to_owned()
        );
    }

    #[test]
    fn gitignore_negation_reincludes_matched_file() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "data/keep.log", b"keep");
        write_file(tmp.path(), "data/drop.log", b"drop");
        write_file(tmp.path(), ".gitignore", b"*.log\n!keep.log\n");

        let result = hash_directory(tmp.path(), true).unwrap();
        let names: Vec<String> = result
            .manifest
            .iter()
            .map(|e| path_to_nfc_string(&e.path))
            .collect();
        assert!(names.iter().any(|n| n.ends_with("keep.log")));
        assert!(!names.iter().any(|n| n.ends_with("drop.log")));
    }
}
