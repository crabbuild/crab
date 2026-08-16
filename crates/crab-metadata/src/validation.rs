use crate::error::{MetadataError, Result};

pub(crate) fn validate_content_hash(value: &str, field: &str, path: &str) -> Result<()> {
    validate_hex_component(value, field, path, 64)
}

pub(crate) fn validate_sha1(value: &str, field: &str, path: &str) -> Result<()> {
    validate_hex_component(value, field, path, 40)
}

pub(crate) fn corrupt_object(path: &str, reason: impl Into<String>) -> MetadataError {
    MetadataError::CorruptObject {
        path: path.to_owned(),
        reason: reason.into(),
    }
}

fn validate_hex_component(value: &str, field: &str, path: &str, len: usize) -> Result<()> {
    if value.len() != len || !value.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(corrupt_object(
            path,
            format!("{field} must be {len} hex characters"),
        ));
    }
    Ok(())
}
