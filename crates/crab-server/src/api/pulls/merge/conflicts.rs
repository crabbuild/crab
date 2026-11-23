use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::tree::Error;

const MAX_FILES: usize = 8;
const MAX_FILE_BYTES: usize = 128 * 1024;
const MAX_TOTAL_BYTES: usize = 512 * 1024;
pub(in crate::api::pulls) const MAX_REQUEST_BYTES: usize = 4 * 1024 * 1024;

#[derive(Serialize)]
pub(in crate::api::pulls) struct File {
    path: String,
    path_hex: String,
    text: String,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(in crate::api::pulls) struct Edit {
    path_hex: String,
    text: String,
}

#[derive(Default)]
pub(super) enum Mode {
    #[default]
    Reject,
    Preview(Vec<File>),
    Apply(BTreeMap<Vec<u8>, String>),
}

impl Mode {
    pub(super) fn apply(edits: Vec<Edit>) -> Result<Self, Error> {
        if edits.is_empty()
            || edits.len() > MAX_FILES
            || edits.iter().map(|edit| edit.text.len()).sum::<usize>() > MAX_TOTAL_BYTES
        {
            return Err(Error::Resolution(
                "Submit all conflicting files within the browser size limit",
            ));
        }
        let mut files = BTreeMap::new();
        for edit in edits {
            let path = crate::api::decode_hex(&edit.path_hex)
                .ok()
                .filter(|path| !path.is_empty() && path.len() <= 1024)
                .ok_or(Error::Resolution("Invalid conflicting file path"))?;
            if edit.text.len() > MAX_FILE_BYTES || edit.text.contains('\0') {
                return Err(Error::Resolution(
                    "Resolved files must be UTF-8 text of at most 128 KiB",
                ));
            }
            if edit.text.lines().any(|line| {
                line.starts_with("<<<<<<< ") || line == "=======" || line.starts_with(">>>>>>> ")
            }) {
                return Err(Error::Resolution(
                    "Remove all conflict markers before committing",
                ));
            }
            if files.insert(path, edit.text).is_some() {
                return Err(Error::Resolution("Resolve each conflicting file once"));
            }
        }
        Ok(Self::Apply(files))
    }

    pub(super) fn resolve(&mut self, path: &[u8], marked: Vec<u8>) -> Result<Vec<u8>, Error> {
        if marked.len() > MAX_FILE_BYTES || path.len() > 1024 {
            return Err(Error::Conflict);
        }
        let text = String::from_utf8(marked).map_err(|_| Error::Conflict)?;
        match self {
            Self::Reject => Err(Error::Conflict),
            Self::Preview(files) => {
                if files.len() >= MAX_FILES
                    || files.iter().map(|file| file.text.len()).sum::<usize>() + text.len()
                        > MAX_TOTAL_BYTES
                {
                    return Err(Error::Conflict);
                }
                files.push(File {
                    path: crate::api::display_path(path),
                    path_hex: crate::api::encode_hex(path),
                    text,
                });
                // Preview does not publish its temporary objects.
                Ok(Vec::new())
            }
            Self::Apply(edits) => {
                edits
                    .remove(path)
                    .map(String::into_bytes)
                    .ok_or(Error::Resolution(
                        "Resolve every conflicting file before committing",
                    ))
            }
        }
    }

    pub(super) fn check_complete(&self) -> Result<(), Error> {
        match self {
            Self::Apply(edits) if !edits.is_empty() => Err(Error::Resolution(
                "Resolutions can only modify conflicting files",
            )),
            _ => Ok(()),
        }
    }
}
