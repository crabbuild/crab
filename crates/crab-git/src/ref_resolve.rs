//! Git ref-name to SHA resolution.
//!
//! This Module owns local ref-store resolution without depending on the Crab
//! CLI error taxonomy. CLI callers map [`RefResolveError`] at the command seam.

use std::collections::HashMap;
use std::path::Path;

use gix_ref::Reference;
use gix_ref::file::Store;

use crate::discover::{discover_git_dir, resolve_common_dir};

/// Result alias for Git ref resolution.
pub type Result<T> = std::result::Result<T, RefResolveError>;

/// Errors returned while resolving Git refs.
#[derive(thiserror::Error, Debug)]
#[non_exhaustive]
pub enum RefResolveError {
    /// A ref-store lookup failed while resolving a named ref.
    #[error("ref store error for '{name}': {source}")]
    RefStore {
        name: String,
        #[source]
        source: gix_ref::file::find::Error,
    },

    /// A typed ref lookup failed and the caller needs the original gix error.
    #[error("gix ref: {source}")]
    TypedRefStore {
        #[source]
        source: gix_ref::file::find::Error,
    },

    /// The requested ref was not present in the ref store.
    #[error("ref not found: '{name}'")]
    NotFound { name: String },

    /// A symbolic ref pointed at another ref that was missing.
    #[error("symbolic ref '{name}' points at missing target '{target}'")]
    MissingSymbolicTarget { name: String, target: String },

    /// Symbolic-ref traversal exceeded the safety limit.
    #[error("symbolic ref chain exceeded 10 levels starting at '{name}' (likely a cycle)")]
    SymbolicCycle { name: String },
}

/// Resolve a list of ref names or SHAs to hex SHAs.
pub fn resolve_refs_batch(refs: &[&str]) -> Result<HashMap<String, String>> {
    if refs.is_empty() {
        return Ok(HashMap::new());
    }

    let git_dir = discover_git_dir();
    let store = open_ref_store(&git_dir);

    let mut out = HashMap::with_capacity(refs.len());
    for name in refs {
        let sha = resolve_one(&store, name)?;
        out.insert((*name).to_owned(), sha);
    }
    Ok(out)
}

/// Resolve multiple refs against an explicit git directory.
pub fn resolve_refs_batch_at(git_dir: &Path, refs: &[&str]) -> Result<HashMap<String, String>> {
    if refs.is_empty() {
        return Ok(HashMap::new());
    }

    refs.iter()
        .map(|name| resolve_ref_at(git_dir, name).map(|sha| ((*name).to_owned(), sha)))
        .collect()
}

/// Resolve a single ref name or SHA to a hex SHA.
pub fn resolve_ref(name: &str) -> Result<String> {
    let git_dir = discover_git_dir();
    let store = open_ref_store(&git_dir);
    resolve_one(&store, name)
}

/// Resolve a single ref name or SHA against an explicit git directory.
pub fn resolve_ref_at(git_dir: &Path, name: &str) -> Result<String> {
    let common_dir = resolve_common_dir(git_dir);
    let store = open_ref_store(&common_dir);

    if name == "HEAD" || name == "head" {
        let head_file = git_dir.join("HEAD");
        if let Ok(content) = std::fs::read_to_string(&head_file) {
            let content = content.trim();
            if let Some(ref_name) = content.strip_prefix("ref: ") {
                return resolve_one(&store, ref_name);
            }
            if content.len() == 40 && content.chars().all(|c| c.is_ascii_hexdigit()) {
                return Ok(content.to_ascii_lowercase());
            }
        }
    }

    resolve_one(&store, name)
}

/// Resolve a list of ref names to typed [`gix_ref::Reference`] values.
pub fn resolve_refs_typed_batch(refs: &[&str]) -> Result<HashMap<String, Reference>> {
    if refs.is_empty() {
        return Ok(HashMap::new());
    }

    let git_dir = discover_git_dir();
    let store = open_ref_store(&git_dir);

    let mut out = HashMap::with_capacity(refs.len());
    for name in refs {
        if let Some(reference) = store
            .try_find(*name)
            .map_err(|source| RefResolveError::TypedRefStore { source })?
        {
            out.insert((*name).to_owned(), reference);
        }
    }
    Ok(out)
}

/// Resolve a single ref name to a typed [`gix_ref::Reference`].
pub fn resolve_ref_typed(name: &str) -> Result<Option<Reference>> {
    let git_dir = discover_git_dir();
    let store = open_ref_store(&git_dir);
    store
        .try_find(name)
        .map_err(|source| RefResolveError::TypedRefStore { source })
}

/// Resolve the local repository's `HEAD` symbolic ref target.
///
/// Returns `Ok(None)` when `HEAD` is detached or missing. Callers own fallback
/// policy such as default branch selection.
pub fn resolve_head_symref() -> Result<Option<String>> {
    let git_dir = discover_git_dir();
    resolve_head_symref_at(&git_dir)
}

/// Resolve `HEAD` as a symbolic ref against an explicit git directory.
///
/// This intentionally opens `git_dir`, not the common dir, so linked worktrees
/// observe their own `HEAD` rather than the main worktree's `HEAD`.
pub fn resolve_head_symref_at(git_dir: &Path) -> Result<Option<String>> {
    let store = open_ref_store(git_dir);
    let Some(reference) = store
        .try_find("HEAD")
        .map_err(|source| RefResolveError::TypedRefStore { source })?
    else {
        return Ok(None);
    };

    match reference.target {
        gix_ref::Target::Symbolic(inner) => Ok(Some(inner.to_string())),
        gix_ref::Target::Object(_) => Ok(None),
    }
}

pub(crate) fn open_ref_store(git_dir: &Path) -> Store {
    Store::at(
        git_dir.to_path_buf(),
        gix_ref::store::init::Options {
            write_reflog: gix_ref::store::WriteReflog::Disable,
            object_hash: gix_hash::Kind::Sha1,
            ..Default::default()
        },
    )
}

fn resolve_one(store: &Store, name: &str) -> Result<String> {
    if name.len() == 40 && name.chars().all(|c| c.is_ascii_hexdigit()) {
        return Ok(name.to_ascii_lowercase());
    }

    let reference = store
        .try_find(name)
        .map_err(|source| RefResolveError::RefStore {
            name: name.to_owned(),
            source,
        })?
        .ok_or_else(|| RefResolveError::NotFound {
            name: name.to_owned(),
        })?;

    let mut current = reference;
    for _ in 0..10 {
        match current.target {
            gix_ref::Target::Object(oid) => return Ok(oid.to_hex().to_string()),
            gix_ref::Target::Symbolic(ref inner_name) => {
                let target = inner_name.to_string();
                let next = store
                    .try_find(inner_name)
                    .map_err(|source| RefResolveError::RefStore {
                        name: target.clone(),
                        source,
                    })?
                    .ok_or_else(|| RefResolveError::MissingSymbolicTarget {
                        name: name.to_owned(),
                        target,
                    })?;
                current = next;
            }
        }
    }

    Err(RefResolveError::SymbolicCycle {
        name: name.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passthrough_hex_sha() {
        let sha = "a".repeat(40);
        let opened = open_ref_store(&std::env::temp_dir());
        let out = resolve_one(&opened, &sha).expect("passthrough");
        assert_eq!(out, sha);
    }

    #[test]
    fn mixed_case_hex_is_normalized() {
        let sha = "AbCdEf".repeat(6) + "abcd";
        let opened = open_ref_store(&std::env::temp_dir());
        let out = resolve_one(&opened, &sha).expect("passthrough");
        assert_eq!(out, sha.to_ascii_lowercase());
    }

    #[test]
    fn resolve_ref_at_follows_head_symbolic_ref() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let git_dir = tmp.path().join(".git");
        let refs_dir = git_dir.join("refs").join("heads");
        std::fs::create_dir_all(&refs_dir).expect("refs");
        std::fs::write(git_dir.join("HEAD"), b"ref: refs/heads/main\n").expect("head");
        std::fs::write(
            refs_dir.join("main"),
            b"1111111111111111111111111111111111111111\n",
        )
        .expect("main");

        let resolved = resolve_ref_at(&git_dir, "HEAD").expect("head");
        assert_eq!(resolved, "1111111111111111111111111111111111111111");
    }

    #[test]
    fn resolve_head_symref_at_returns_target_for_attached_head() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let git_dir = tmp.path().join(".git");
        std::fs::create_dir_all(git_dir.join("refs").join("heads")).expect("refs");
        std::fs::write(git_dir.join("HEAD"), b"ref: refs/heads/main\n").expect("head");

        let resolved = resolve_head_symref_at(&git_dir).expect("head");
        assert_eq!(resolved.as_deref(), Some("refs/heads/main"));
    }

    #[test]
    fn resolve_head_symref_at_returns_none_for_detached_head() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let git_dir = tmp.path().join(".git");
        std::fs::create_dir_all(&git_dir).expect("git");
        std::fs::write(
            git_dir.join("HEAD"),
            b"1111111111111111111111111111111111111111\n",
        )
        .expect("head");

        let resolved = resolve_head_symref_at(&git_dir).expect("head");
        assert_eq!(resolved, None);
    }

    #[test]
    fn missing_ref_is_structured() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let git_dir = tmp.path().join(".git");
        std::fs::create_dir_all(git_dir.join("refs").join("heads")).expect("refs");
        std::fs::write(git_dir.join("HEAD"), b"ref: refs/heads/main\n").expect("head");

        let err = resolve_ref_at(&git_dir, "refs/heads/missing").unwrap_err();
        assert!(matches!(err, RefResolveError::NotFound { name } if name == "refs/heads/missing"));
    }
}
