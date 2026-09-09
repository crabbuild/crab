use crate::{Error, ErrorKind, Result};

/// Explicit Azure placement and static authorization; no default credential chain is used.
#[derive(Clone)]
#[cfg_attr(
    not(feature = "remote"),
    expect(dead_code, reason = "Configuration is consumed by remote builds")
)]
pub struct AzureOptions {
    pub(super) account: String,
    pub(super) container: String,
    token: Token,
    endpoint: Option<String>,
}

#[derive(Clone)]
#[cfg_attr(
    not(feature = "remote"),
    expect(dead_code, reason = "Configuration is consumed by remote builds")
)]
enum Token {
    Bearer(String),
    Sas(String),
}

impl std::fmt::Debug for AzureOptions {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AzureOptions")
            .finish_non_exhaustive()
    }
}

impl AzureOptions {
    /// Select a nonempty bearer token containing only visible ASCII characters.
    pub fn bearer(account: &str, container: &str, token: &str) -> Result<Self> {
        // The provider constructs an HTTP header with an unchecked conversion.
        // Reject invalid header bytes before any request can reach that path.
        if token.is_empty() || !token.bytes().all(|byte| byte.is_ascii_graphic()) {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "Azure bearer token must contain only visible ASCII characters",
            ));
        }
        Self::new(account, container, Token::Bearer(token.to_owned()))
    }

    /// Select a nonempty SAS query string, parsed by the provider at client build.
    ///
    /// Supply the token as issued, with optional leading `?`, not a full URL.
    pub fn sas(account: &str, container: &str, token: &str) -> Result<Self> {
        if token.trim_start_matches('?').trim().is_empty() || token.contains("://") {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "Azure SAS must be a nonempty query string",
            ));
        }
        Self::new(account, container, Token::Sas(token.to_owned()))
    }

    fn new(account: &str, container: &str, token: Token) -> Result<Self> {
        if !super::valid_cloud_name(account) || !super::valid_cloud_name(container) {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "Azure requires bare account and container names",
            ));
        }
        Ok(Self {
            account: account.to_owned(),
            container: container.to_owned(),
            token,
            endpoint: None,
        })
    }

    /// Select an HTTP(S) endpoint, validated by the provider when building.
    ///
    /// Selecting HTTP explicitly permits plaintext transport for this store.
    /// Credentials, query strings and fragments in the endpoint are rejected.
    #[must_use]
    pub fn with_endpoint(mut self, endpoint: &str) -> Self {
        self.endpoint = Some(endpoint.trim().to_owned());
        self
    }

    #[cfg(feature = "remote")]
    pub(super) fn build(self) -> Result<(crab_storage::Store, String)> {
        let mut namespace = blake3::Hasher::new_derive_key("crab SDK explicit Azure cache v1");
        // Bearer and SAS credentials may grant different scopes on one target.
        // Include the authorization kind and value before the transport identity.
        let token = match self.token {
            Token::Bearer(value) => {
                namespace.update(b"bearer\0");
                namespace.update(value.as_bytes());
                crab_storage::AzureAuthorization::Bearer(value)
            }
            Token::Sas(value) => {
                namespace.update(b"sas\0");
                namespace.update(value.as_bytes());
                crab_storage::AzureAuthorization::Sas(value)
            }
        };
        let credentials = crab_storage::ObjectStoreCredentials::Azure {
            account: self.account,
            token,
        };
        super::build_explicit(
            &self.container,
            credentials,
            self.endpoint.as_deref(),
            namespace,
        )
    }

    #[cfg(feature = "local")]
    pub(super) fn local_transport(
        &self,
        locator: &crate::RepositoryLocator,
    ) -> Result<super::LocalTransport> {
        let mut environment = vec![
            ("CRAB_STORAGE_PROVIDER".into(), "azure".into()),
            (
                "AZURE_STORAGE_ACCOUNT_NAME".into(),
                self.account.clone().into(),
            ),
        ];
        match &self.token {
            Token::Bearer(token) => {
                environment.push(("AZURE_STORAGE_TOKEN".into(), token.clone().into()));
            }
            Token::Sas(token) => {
                environment.push(("AZURE_STORAGE_SAS_KEY".into(), token.clone().into()));
            }
        }
        if let Some(endpoint) = &self.endpoint {
            environment.push(("AZURE_STORAGE_ENDPOINT".into(), endpoint.clone().into()));
        }
        Ok(super::LocalTransport {
            remote: format!(
                "crab://{}/{}/{}",
                self.account,
                self.container,
                locator.direct_prefix()?
            ),
            environment,
        })
    }
}

#[cfg(all(test, feature = "remote"))]
mod tests {
    use super::*;

    #[test]
    fn azure_cache_scope_binds_authorization_kind_and_token() {
        let first = AzureOptions::bearer("account", "container", "sig=first")
            .unwrap()
            .build()
            .unwrap();
        for options in [
            AzureOptions::bearer("account", "container", "sig=other").unwrap(),
            AzureOptions::sas("account", "container", "sig=first").unwrap(),
        ] {
            let other = options.build().unwrap();
            assert_eq!(first.0.target_identity(), other.0.target_identity());
            assert_ne!(first.1, other.1);
        }
    }
}
