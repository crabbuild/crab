use percent_encoding::percent_decode_str;

const MAX_KEY_BYTES: usize = 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ObjectAddress {
    pub reference: String,
    pub branch: Option<String>,
    pub path: crab_remote_git::GitPath,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NamespaceError {
    MissingReference,
    MissingObject,
    InvalidReference,
    InvalidPath,
}

pub(crate) fn object_address(key: &str) -> Result<ObjectAddress, NamespaceError> {
    if key.len() > MAX_KEY_BYTES {
        return Err(NamespaceError::InvalidPath);
    }
    let (encoded_ref, path) = key.split_once('/').ok_or(NamespaceError::MissingObject)?;
    if encoded_ref.is_empty() {
        return Err(NamespaceError::MissingReference);
    }
    let reference = percent_decode_str(encoded_ref)
        .decode_utf8()
        .map_err(|_| NamespaceError::InvalidReference)?;
    let (reference, branch) = resolve_reference(&reference)?;
    validate_path(path)?;
    let path = crab_remote_git::GitPath::new(path.as_bytes().to_vec())
        .map_err(|_| NamespaceError::InvalidPath)?;
    Ok(ObjectAddress {
        reference,
        branch,
        path,
    })
}

pub(crate) fn listing_reference(prefix: &str) -> Result<Option<(String, String)>, NamespaceError> {
    let Some((encoded_ref, path_prefix)) = prefix.split_once('/') else {
        if prefix.is_empty() {
            return Ok(None);
        }
        return Err(NamespaceError::MissingReference);
    };
    if encoded_ref.is_empty() {
        return Err(NamespaceError::MissingReference);
    }
    let reference = percent_decode_str(encoded_ref)
        .decode_utf8()
        .map_err(|_| NamespaceError::InvalidReference)?;
    let (reference, _) = resolve_reference(&reference)?;
    if prefix.len() > MAX_KEY_BYTES
        || path_prefix.as_bytes().contains(&0)
        || path_prefix.starts_with('/')
        || path_prefix.contains("//")
    {
        return Err(NamespaceError::InvalidPath);
    }
    Ok(Some((reference, path_prefix.to_owned())))
}

fn resolve_reference(value: &str) -> Result<(String, Option<String>), NamespaceError> {
    if value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Ok((value.to_ascii_lowercase(), None));
    }
    if let Some(name) = value.strip_prefix("refs/heads/") {
        validate_branch(name)?;
        return Ok((value.to_owned(), Some(value.to_owned())));
    }
    if let Some(name) = value.strip_prefix("refs/tags/") {
        validate_ref(value, name)?;
        return Ok((value.to_owned(), None));
    }
    validate_branch(value)?;
    let reference = format!("refs/heads/{value}");
    Ok((reference.clone(), Some(reference)))
}

fn validate_branch(value: &str) -> Result<(), NamespaceError> {
    let reference = format!("refs/heads/{value}");
    validate_ref(&reference, value)
}

fn validate_ref(reference: &str, name: &str) -> Result<(), NamespaceError> {
    if name.is_empty()
        || name.contains(['~', '^'])
        || name.contains("@{")
        || crab_git::validate_push_refname(reference).is_err()
    {
        return Err(NamespaceError::InvalidReference);
    }
    Ok(())
}

fn validate_path(path: &str) -> Result<(), NamespaceError> {
    if path.is_empty()
        || path.len() > MAX_KEY_BYTES
        || path.starts_with('/')
        || path.ends_with('/')
        || path.contains("//")
        || path
            .bytes()
            .any(|byte| byte == 0 || byte.is_ascii_control())
        || path.split('/').any(|component| {
            component.is_empty()
                || component.len() > 255
                || matches!(component, "." | "..")
                || component.eq_ignore_ascii_case(".git")
        })
    {
        return Err(NamespaceError::InvalidPath);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoded_branch_slash_is_decoded_once() {
        let address = object_address("feature%2Fdata/a%2Fb").unwrap();
        assert_eq!(address.reference, "refs/heads/feature/data");
        assert_eq!(address.path.as_bytes(), b"a%2Fb");
    }

    #[test]
    fn ancestry_and_lossy_paths_are_rejected() {
        for key in ["main/a//b", "main/a/../b", "main/.git/config", "main/"] {
            assert!(object_address(key).is_err(), "{key}");
        }
        for key in ["main~/a", "main^/a", "main@{1}/a"] {
            assert!(object_address(key).is_err(), "{key}");
        }
    }
}
