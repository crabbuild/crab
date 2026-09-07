//! Git reference-name validation shared by push entry points.

use bstr::ByteSlice;
use thiserror::Error;

/// A ref cannot coexist with another ref beneath its slash-delimited name.
#[derive(Debug, Error)]
#[error("ref {name} conflicts with ref {ancestor}")]
pub struct RefNamespaceError {
    name: String,
    ancestor: String,
}

/// Validate the complete candidate set after applying all ref creations/deletions.
///
/// Individual names must already be validated. An atomic replacement of a ref
/// with descendants is valid when the old ref is absent from the final set.
pub fn validate_ref_namespace<'a>(
    names: impl IntoIterator<Item = &'a str>,
) -> Result<(), RefNamespaceError> {
    let names: std::collections::BTreeSet<_> = names.into_iter().collect();
    for name in &names {
        for (index, _) in name.match_indices('/') {
            let ancestor = &name[..index];
            if names.contains(ancestor) {
                return Err(RefNamespaceError {
                    name: (*name).to_owned(),
                    ancestor: ancestor.to_owned(),
                });
            }
        }
    }
    Ok(())
}

/// A Git reference name rejected by Git's canonical validation rules.
#[derive(Debug, Error)]
#[error("invalid ref name: {name}")]
pub struct RefNameError {
    name: String,
}

/// Validate a push refspec reference name via `gix-validate`.
///
/// Accepts both fully-qualified refs (`refs/heads/main`) and one-level
/// names (`main`), matching `git check-ref-format --allow-onelevel`.
pub fn validate_push_refname(name: &str) -> Result<&str, RefNameError> {
    gix_validate::reference::name_partial(name.as_bytes().as_bstr()).map_err(|_| RefNameError {
        name: name.to_owned(),
    })?;
    Ok(name)
}
