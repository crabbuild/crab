//! Git reference-name validation shared by push entry points.

use bstr::ByteSlice;
use thiserror::Error;

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
