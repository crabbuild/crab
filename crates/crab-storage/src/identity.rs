//! Logical bucket identity and credential-free transport endpoint binding.

pub use crab_types::storage::{BucketIdentity, StorageProviderKind};

/// Identify a credential-free HTTP storage authority, preserving case-sensitive paths.
pub fn endpoint_identity(endpoint: &str) -> crate::error::Result<[u8; 32]> {
    let url = url::Url::parse(endpoint).map_err(|source| {
        crate::error::StorageError::InvalidObjectStoreUrl {
            url: "storage endpoint".to_owned(),
            source,
        }
    })?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(crate::error::StorageError::InvalidStaticEnvTarget {
            target: "storage endpoint".to_owned(),
            reason: "expected an HTTP(S) URL without credentials, query or fragment".to_owned(),
        });
    }
    Ok(blake3::derive_key(
        "crab storage endpoint v1",
        url.as_str().as_bytes(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_identity_normalizes_authority_without_folding_paths() {
        let first = endpoint_identity("https://STORE.example:443/Scope").unwrap();
        assert_eq!(
            first,
            endpoint_identity("https://store.example/Scope").unwrap()
        );
        assert_ne!(
            first,
            endpoint_identity("https://store.example/scope").unwrap()
        );
    }

    #[test]
    fn endpoint_identity_rejects_embedded_secrets_without_disclosing_them() {
        for endpoint in [
            "https://user:private-token@store.example",
            "https://store.example?signature=private-token",
            "https://store.example#private-token",
            "file:///private-token",
        ] {
            let error = endpoint_identity(endpoint).unwrap_err();
            assert!(!error.to_string().contains("private-token"));
        }
    }
}
