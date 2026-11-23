//! Viewer authority carried by a negotiated Crab Auth response.

use std::collections::BTreeSet;
use std::time::{Duration, SystemTime};

use crate::error::{AuthError, Result};
use crate::managed::SecretString;
use crab_types::storage::StorageScope;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

const REPOSITORY_DIRECTORIES: &[&str] = &[
    "v2/capsules",
    "v2/checkpoints",
    "v2/catalogs",
    "v2/pack-layers",
    "v2/history",
    "lfs/objects",
];
const GLOBAL_DIRECTORIES: &[&str] = &["xorbs", "shards", "capsule-chunk-pages"];

/// CDN viewer mechanism selected by an authorized issuer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum CdnProvider {
    Cloudfront,
    CloudCdn,
    FrontDoor,
}

/// A short-lived viewer grant whose credentials are redacted in diagnostics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CdnViewerGrant {
    pub provider: CdnProvider,
    pub origin: String,
    pub base_url: String,
    pub repo_prefix: String,
    pub global_prefix: Option<String>,
    pub expires_at: u64,
    grants: Vec<PrefixCredential>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
struct PrefixCredential {
    prefix: String,
    credential: SecretString,
}

impl CdnViewerGrant {
    /// Compare deployment, view and object permissions independently of rotation.
    #[must_use]
    pub fn same_authority(&self, other: &Self) -> bool {
        self.provider == other.provider
            && self.origin == other.origin
            && self.base_url == other.base_url
            && self.repo_prefix == other.repo_prefix
            && self.global_prefix == other.global_prefix
            && self.grants.len() == other.grants.len()
            && self
                .grants
                .iter()
                .all(|a| other.grants.iter().any(|b| a.prefix == b.prefix))
    }

    pub(crate) fn expiry(&self) -> Option<SystemTime> {
        SystemTime::UNIX_EPOCH.checked_add(Duration::from_secs(self.expires_at))
    }

    /// Schedule renewal up to 15 seconds early, using at most half the remaining lifetime.
    #[must_use]
    pub fn refresh_at(&self, captured_at: SystemTime) -> Option<SystemTime> {
        let expiry = self.expiry()?;
        let lifetime = expiry.duration_since(captured_at).unwrap_or_default();
        Some(expiry - Duration::from_secs(15).min(lifetime / 2))
    }

    /// Check the issuer's response against the caller's independently selected deployment.
    pub fn validate_binding(
        &self,
        expected_origin: &str,
        expected_base_url: &str,
        source_repo: &str,
        scope: Option<&StorageScope>,
        parent_expiry: SystemTime,
        now: SystemTime,
    ) -> Result<()> {
        if self.origin != expected_origin || self.base_url != expected_base_url {
            return invalid("CDN viewer grant does not match the selected origin or deployment");
        }
        self.validate_scope(source_repo, scope)?;
        let expiry = self
            .expiry()
            .ok_or_else(|| AuthError::InvalidCredentialResponse("invalid CDN expiry".into()))?;
        if expiry <= now || expiry > parent_expiry {
            return invalid("CDN viewer grant expires outside its parent read authorization");
        }
        Ok(())
    }

    /// Select the opaque credential for the physical key before its expiry.
    #[must_use]
    pub fn viewer_for_object(&self, key: &str, now: SystemTime) -> Option<&SecretString> {
        let expiry = self.expiry()?;
        if now >= expiry {
            return None;
        }
        self.grants
            .iter()
            .find(|grant| {
                key.strip_prefix(&grant.prefix)
                    .is_some_and(|suffix| !suffix.is_empty())
            })
            .map(|grant| &grant.credential)
    }

    pub(crate) fn parse(
        value: serde_json::Value,
        provider: &str,
        scope: Option<&StorageScope>,
    ) -> Result<Self> {
        // Provider errors and deserializer input can contain bearer material.
        let grant: Self = serde_json::from_value(value)
            .map_err(|_| AuthError::InvalidCredentialResponse("invalid CDN viewer grant".into()))?;
        let expected_provider = match provider {
            "aws" => CdnProvider::Cloudfront,
            "gcp" => CdnProvider::CloudCdn,
            "azure" => CdnProvider::FrontDoor,
            _ => return invalid("CDN viewer grant requires a native cloud provider"),
        };
        grant.validate_authority(expected_provider)?;
        grant.validate_scope(
            scope.map_or(grant.repo_prefix.as_str(), |scope| {
                scope.source_repo.as_str()
            }),
            scope,
        )?;
        Ok(grant)
    }

    pub(crate) fn validate_authority(&self, expected_provider: CdnProvider) -> Result<()> {
        if self.provider != expected_provider
            || !canonical_https_base(&self.base_url)
            || !canonical_origin(self.provider, &self.origin)
            || self.grants.is_empty()
            || self.grants.len() > REPOSITORY_DIRECTORIES.len() + GLOBAL_DIRECTORIES.len()
            || !canonical_prefix(&self.repo_prefix)
        {
            return invalid("invalid CDN viewer grant authority");
        }

        let mut seen = BTreeSet::new();
        for item in &self.grants {
            if !seen.insert(item.prefix.as_str())
                || !self.permits_prefix(&item.prefix)
                || !valid_credential(self.provider, item.credential.expose_secret())
            {
                return invalid("invalid or duplicated CDN viewer grant scope");
            }
        }
        Ok(())
    }

    fn permits_prefix(&self, prefix: &str) -> bool {
        if let Some(directory) = scoped_directory(prefix, &self.repo_prefix)
            && REPOSITORY_DIRECTORIES.contains(&directory)
        {
            return true;
        }
        self.global_prefix.as_ref().is_some_and(|global| {
            scoped_directory(prefix, global)
                .is_some_and(|directory| GLOBAL_DIRECTORIES.contains(&directory))
        })
    }

    pub(crate) fn validate_scope(
        &self,
        source_repo: &str,
        scope: Option<&StorageScope>,
    ) -> Result<()> {
        let valid = match scope {
            Some(scope) => {
                scope.source_repo == source_repo
                    && self.repo_prefix == scope.repo_prefix
                    && self.global_prefix.as_deref() == Some(scope.global_prefix.as_str())
                    && scope.global_prefix == format!("{}/.crab", scope.repo_prefix)
                    && self
                        .repo_prefix
                        .starts_with(&format!("{source_repo}/acl-views/"))
            }
            None => self.repo_prefix == source_repo && self.global_prefix.is_none(),
        };
        if valid {
            Ok(())
        } else {
            invalid("CDN viewer grant exceeds storage authorization")
        }
    }
}

fn scoped_directory<'a>(prefix: &'a str, root: &str) -> Option<&'a str> {
    prefix
        .strip_prefix(root)?
        .strip_prefix('/')?
        .strip_suffix('/')
}

pub(crate) fn canonical_prefix(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= 1024
        && path.split('/').all(|part| {
            !matches!(part, "" | "." | "..")
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        })
}

pub(crate) fn valid_approval_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

pub(crate) fn canonical_https_base(value: &str) -> bool {
    let Some(origin) = value.strip_prefix("https://") else {
        return false;
    };
    let (host, prefix) = origin
        .split_once('/')
        .map_or((origin, None), |(host, prefix)| (host, Some(prefix)));
    host.contains('.')
        && host.split('.').all(|label| {
            !label.is_empty()
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label.len() <= 63
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        })
        && prefix.is_none_or(canonical_prefix)
}

pub(crate) fn canonical_origin(provider: CdnProvider, value: &str) -> bool {
    let label = |value: &str, min: usize, max: usize, punctuation: &[u8]| {
        (min..=max).contains(&value.len())
            && value
                .as_bytes()
                .first()
                .is_some_and(u8::is_ascii_alphanumeric)
            && value
                .as_bytes()
                .last()
                .is_some_and(u8::is_ascii_alphanumeric)
            && value
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || punctuation.contains(&b))
    };
    match provider {
        CdnProvider::Cloudfront => value
            .strip_prefix("s3://")
            .is_some_and(|bucket| label(bucket, 3, 63, b".-")),
        CdnProvider::CloudCdn => value
            .strip_prefix("gs://")
            .is_some_and(|bucket| label(bucket, 3, 222, b"._-")),
        CdnProvider::FrontDoor => value
            .strip_prefix("https://")
            .and_then(|rest| rest.split_once(".blob.core.windows.net/"))
            .is_some_and(|(account, container)| {
                label(account, 3, 24, b"") && label(container, 3, 63, b"-")
            }),
    }
}

fn valid_credential(provider: CdnProvider, value: &str) -> bool {
    match provider {
        CdnProvider::Cloudfront => {
            let mut pairs = value.split("; ");
            for name in [
                "CloudFront-Policy",
                "CloudFront-Signature",
                "CloudFront-Key-Pair-Id",
            ] {
                if !pairs
                    .next()
                    .and_then(|pair| pair.split_once('='))
                    .is_some_and(|(key, value)| {
                        key == name
                            && !value.is_empty()
                            && value.bytes().all(|byte| {
                                byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'~')
                            })
                    })
                {
                    return false;
                }
            }
            pairs.next() == Some("CloudFront-Hash-Algorithm=SHA256") && pairs.next().is_none()
        }
        CdnProvider::CloudCdn => {
            value.starts_with("Cloud-CDN-Cookie=URLPrefix=")
                && value.bytes().all(|byte| {
                    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'=' | b':')
                })
        }
        CdnProvider::FrontDoor => {
            let mut length = 0;
            for component in value.split('.') {
                if component.is_empty()
                    || !component
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
                {
                    return false;
                }
                length += 1;
            }
            length == 3
        }
    }
}

fn invalid<T>(reason: &str) -> Result<T> {
    Err(AuthError::InvalidCredentialResponse(reason.into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credential_response::parse_credential_response;
    use serde_json::json;

    fn viewer_response() -> serde_json::Value {
        json!({
            "provider": "aws",
            "credentials": {},
            "expires_at": "2026-12-01T00:00:00Z",
            "permissions": ["read"],
            "cdn": {
                "provider": "cloudfront",
                "origin": "s3://bucket",
                "base_url": "https://downloads.example.com",
                "repo_prefix": "org/repo",
                "global_prefix": null,
                "expires_at": 110,
                "grants": [{
                    "prefix": "org/repo/v2/pack-layers/",
                    "credential": "CloudFront-Policy=opaque123; CloudFront-Signature=456; CloudFront-Key-Pair-Id=AB1; CloudFront-Hash-Algorithm=SHA256"
                }]
            }
        })
    }

    #[test]
    fn credential_response_validates_binding_scope_and_expiry() {
        let response = parse_credential_response(&viewer_response().to_string()).unwrap();
        let grant = response.cdn.unwrap();
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
        let parent_expiry = now + Duration::from_secs(30);
        grant
            .validate_binding(
                "s3://bucket",
                "https://downloads.example.com",
                "org/repo",
                None,
                parent_expiry,
                now,
            )
            .unwrap();
        assert!(
            grant
                .viewer_for_object("org/repo/v2/pack-layers/ab/hash", now)
                .is_some()
        );
        assert!(
            grant
                .viewer_for_object("org/repo/v2/pack-layers-neighbor/ab/hash", now)
                .is_none()
        );
        assert!(
            grant
                .viewer_for_object(".crab/xorbs/ab/hash", now)
                .is_none()
        );
        assert!(
            grant
                .viewer_for_object("org/repo/v2/pack-layers/ab/hash", parent_expiry)
                .is_none()
        );
        assert!(
            grant
                .validate_binding(
                    "s3://different-bucket",
                    "https://downloads.example.com",
                    "org/repo",
                    None,
                    parent_expiry,
                    now,
                )
                .is_err()
        );
        assert!(
            grant
                .validate_binding(
                    "s3://bucket",
                    "https://other.example.com",
                    "org/repo",
                    None,
                    parent_expiry,
                    now,
                )
                .is_err()
        );
        assert!(
            grant
                .validate_binding(
                    "s3://bucket",
                    "https://downloads.example.com",
                    "org/repo",
                    None,
                    now,
                    now,
                )
                .is_err()
        );
        assert!(!format!("{grant:?}").contains("opaque123"));
    }

    #[test]
    fn credential_response_rejects_grants_that_widen_view() {
        let mut response = viewer_response();
        let repo = "org/repo/acl-views/v2/view";
        response["storage_scope"] = json!({
            "repo_prefix": repo,
            "source_repo": "org/repo",
            "global_prefix": format!("{repo}/.crab"),
            "scope_hash": "ab".repeat(32),
        });
        assert!(parse_credential_response(&response.to_string()).is_err());

        response["cdn"]["repo_prefix"] = json!(repo);
        response["cdn"]["global_prefix"] = json!(format!("{repo}/.crab"));
        response["cdn"]["grants"][0]["prefix"] = json!(format!("{repo}/.crab/xorbs/"));
        assert!(parse_credential_response(&response.to_string()).is_ok());

        response["cdn"]["grants"][0]["prefix"] = json!(".crab/xorbs/");
        assert!(parse_credential_response(&response.to_string()).is_err());
    }
}
