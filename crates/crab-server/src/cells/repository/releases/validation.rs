use super::*;

pub(super) fn validate_create(input: &CreateReleaseInput) -> crate::cells::Result<()> {
    validate_author(&input.author)?;
    validate_tag_name(&input.tag_name)?;
    validate_oid(&input.target_oid)?;
    validate_title(&input.title)?;
    validate_body(&input.body, false)
}

pub(super) fn validate_tag_name(value: &str) -> crate::cells::Result<()> {
    if value.is_empty()
        || value.len() > 255
        || value.trim() != value
        || value.starts_with("refs/")
        || value.chars().any(char::is_control)
        || crab_git::validate_push_refname(&format!("refs/tags/{value}")).is_err()
    {
        return Err(crate::cells::Error::Invalid(
            "repository release tag is invalid",
        ));
    }
    Ok(())
}

pub(super) fn validate_oid(value: &str) -> crate::cells::Result<()> {
    if value.len() != 40
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        || value.bytes().all(|byte| byte == b'0')
    {
        return Err(crate::cells::Error::Invalid(
            "repository release object ID is invalid",
        ));
    }
    Ok(())
}

pub(super) fn validate_reservation(value: &ReleaseAssetReservation) -> crate::cells::Result<()> {
    validate_number(value.release)?;
    validate_number(value.expected_version)?;
    validate_author(&value.uploader)?;
    if value.name.is_empty()
        || value.name.len() > 255
        || !value.name.is_ascii()
        || value.name.starts_with('.')
        || value.name.ends_with('.')
        || value
            .name
            .bytes()
            .any(|byte| byte.is_ascii_control() || matches!(byte, b'/' | b'\\' | b'"'))
        || value.content_type.is_empty()
        || value.content_type.len() > 255
        || value.content_type.chars().any(char::is_control)
    {
        return Err(crate::cells::Error::Invalid(
            "repository release asset reservation is invalid",
        ));
    }
    Ok(())
}

pub(super) fn validate_asset_result(size: u64, digest: &str) -> crate::cells::Result<()> {
    if size > crate::server::MAX_DEPENDENCY_FILE_BYTES
        || digest.len() != 64
        || !digest
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(crate::cells::Error::Invalid(
            "repository release asset result is invalid",
        ));
    }
    Ok(())
}

pub(super) fn release_digest(input: &CreateReleaseInput) -> blake3::Hash {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"crab.repository.release-submission.v1\0");
    hash_text(&mut hasher, &input.author.issuer);
    hash_text(&mut hasher, &input.author.subject);
    for value in [
        &input.tag_name,
        &input.target_oid,
        &input.title,
        &input.body,
    ] {
        hash_text(&mut hasher, value);
    }
    hasher.update(&[u8::from(input.prerelease), u8::from(input.draft)]);
    hasher.finalize()
}

pub(super) fn asset_reservation_digest(value: &ReleaseAssetReservation) -> blake3::Hash {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"crab.repository.release-asset-reservation.v1\0");
    hasher.update(&value.release.to_be_bytes());
    hasher.update(&value.expected_version.to_be_bytes());
    hash_text(&mut hasher, &value.name);
    hash_text(&mut hasher, &value.content_type);
    hash_text(&mut hasher, &value.uploader.issuer);
    hash_text(&mut hasher, &value.uploader.subject);
    hasher.finalize()
}

pub(super) fn next_version(value: u64) -> crate::cells::Result<u64> {
    value
        .checked_add(1)
        .filter(|value| *value < MAX_NUMBER)
        .ok_or(crate::cells::Error::Invalid(
            "repository release version is exhausted",
        ))
}

pub(super) fn publication_result_version(expected_version: u64) -> crate::cells::Result<u64> {
    if expected_version == 0 {
        Ok(1)
    } else {
        next_version(expected_version)
    }
}

pub(super) fn validate_query(query: Option<&str>) -> crate::cells::Result<()> {
    if query.is_some_and(|query| {
        query.is_empty()
            || query.trim() != query
            || query.chars().count() > 256
            || query.chars().any(char::is_control)
            || query.to_lowercase() != query
    }) {
        return Err(crate::cells::Error::Invalid(
            "repository search query is invalid",
        ));
    }
    Ok(())
}
