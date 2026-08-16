//! Auth provider identity and token-cache contracts.

use serde::{Deserialize, Serialize};

/// Auth provider kind shared by CLI, SDK, and auth clients.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AuthProviderKind {
    /// AWS STS via OIDC token exchange.
    AwsOidc,
    /// GCP Workload Identity Federation.
    GcpWorkloadIdentity,
    /// Azure Entra ID.
    AzureEntra,
    /// Crab Auth enterprise service.
    CrabAuth,
    /// Static credentials from environment.
    #[default]
    Static,
    /// No auth.
    None,
}

impl AuthProviderKind {
    /// Stable config and token-cache label.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AwsOidc => "aws-oidc",
            Self::GcpWorkloadIdentity => "gcp-workload-identity",
            Self::AzureEntra => "azure-entra",
            Self::CrabAuth => "crab-auth",
            Self::Static => "static",
            Self::None => "none",
        }
    }

    /// Parses a stable auth provider label.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "aws-oidc" => Some(Self::AwsOidc),
            "gcp-workload-identity" => Some(Self::GcpWorkloadIdentity),
            "azure-entra" => Some(Self::AzureEntra),
            "crab-auth" => Some(Self::CrabAuth),
            "static" => Some(Self::Static),
            "none" => Some(Self::None),
            _ => None,
        }
    }

    /// Token-cache keys accepted for this provider.
    ///
    /// The first key is canonical. Alternate keys can be added here only for a
    /// shipped token-cache migration.
    #[must_use]
    pub fn token_cache_keys(self) -> &'static [&'static str] {
        match self {
            Self::AwsOidc => &["aws-oidc"],
            Self::GcpWorkloadIdentity => &["gcp-workload-identity"],
            Self::AzureEntra => &["azure-entra"],
            Self::CrabAuth => &["crab-auth"],
            Self::Static => &["static"],
            Self::None => &["none"],
        }
    }

    /// Returns whether this provider writes OIDC tokens to the token cache.
    #[must_use]
    pub fn uses_token_cache(self) -> bool {
        !matches!(self, Self::Static | Self::None)
    }
}

/// Splits an OAuth scope string into individual scopes.
#[must_use]
pub fn parse_scope_list(scopes: &str) -> Vec<String> {
    scopes.split_ascii_whitespace().map(str::to_owned).collect()
}

impl std::fmt::Display for AuthProviderKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_parse_back_to_provider_kinds() {
        for provider in [
            AuthProviderKind::AwsOidc,
            AuthProviderKind::GcpWorkloadIdentity,
            AuthProviderKind::AzureEntra,
            AuthProviderKind::CrabAuth,
            AuthProviderKind::Static,
            AuthProviderKind::None,
        ] {
            assert_eq!(AuthProviderKind::parse(provider.as_str()), Some(provider));
        }
    }

    #[test]
    fn token_cache_keys_use_canonical_label_first() {
        for provider in [
            AuthProviderKind::AwsOidc,
            AuthProviderKind::GcpWorkloadIdentity,
            AuthProviderKind::AzureEntra,
            AuthProviderKind::CrabAuth,
            AuthProviderKind::Static,
            AuthProviderKind::None,
        ] {
            assert_eq!(
                provider.token_cache_keys().first().copied(),
                Some(provider.as_str())
            );
        }
    }

    #[test]
    fn static_and_none_do_not_use_token_cache() {
        assert!(!AuthProviderKind::Static.uses_token_cache());
        assert!(!AuthProviderKind::None.uses_token_cache());
        assert!(AuthProviderKind::CrabAuth.uses_token_cache());
    }

    #[test]
    fn parse_scope_list_splits_ascii_whitespace() {
        assert_eq!(
            parse_scope_list("openid email\tprofile\nrepo"),
            vec!["openid", "email", "profile", "repo"]
        );
        assert!(parse_scope_list("   ").is_empty());
    }
}
