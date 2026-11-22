//! Nonsecret direct CDN binding scoped to an SDK checkout subprocess.

use std::path::{Path, PathBuf};

use crab_types::storage::{CdnPublicationFile, StorageScope};
use serde::{Deserialize, Serialize};
use url::Url;

use crate::CrabAuthClientConfig;
use crate::crab_auth_client::CdnViewerDeployment;
use crate::error::{AuthError, Result};

/// Maximum serialized size accepted by either executable.
pub const MAX_BINDING_BYTES: usize = 16 * 1024;

/// Source and physical scope authorized by the SDK before starting checkout.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CdnCheckoutBinding {
    pub worktree: PathBuf,
    pub origin: String,
    pub export_origin: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub publication: Option<CdnPublicationFile>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approval_sha256: Option<String>,
    pub source_repo: String,
    pub base_url: String,
    pub target_identity: [u8; 32],
    pub consumer: CdnCheckoutConsumer,
    pub scope: Option<StorageScope>,
    pub issuer: Option<CdnCheckoutIssuer>,
}

/// Native SDK entry points that can delegate a scoped read to a checkout child.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CdnCheckoutConsumer {
    Sdk,
    Desktop,
}

/// Issuer settings and an opaque fingerprint of the parent read's principal.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CdnCheckoutIssuer {
    pub config: CrabAuthClientConfig,
    pub principal: [u8; 32],
}

impl CdnCheckoutBinding {
    /// Revalidate the serialized deployment and issuer before using the binding.
    pub fn deployment(&self) -> Result<CdnViewerDeployment> {
        let deployment = CdnViewerDeployment::new(&self.origin, &self.source_repo, &self.base_url)?
            .with_approval(self.approval_sha256.as_deref())?;
        match (&self.issuer, &self.export_origin, &self.publication) {
            (Some(issuer), None, None) => validate_issuer(&issuer.config)?,
            (None, Some(export), Some(_))
                if self.scope.is_none() && self.approval_sha256.is_none() =>
            {
                deployment.validate_public_export(export)?;
            }
            _ => return Err(invalid("invalid CDN checkout issuer or public export")),
        }
        Ok(deployment)
    }
}

/// Validate the nonsecret issuer settings shared between the SDK and its child.
pub fn validate_issuer(issuer: &CrabAuthClientConfig) -> Result<()> {
    validate_issuer_urls(&issuer.endpoint, &issuer.issuer_url)?;
    if !Path::new(&issuer.token_cache_path).is_absolute() || issuer.client_id.trim().is_empty() {
        return Err(invalid(
            "CDN issuer requires an absolute token-cache path and an OIDC client ID",
        ));
    }
    Ok(())
}

pub(crate) fn validate_issuer_urls(endpoint: &str, issuer: &str) -> Result<()> {
    for value in [endpoint, issuer] {
        let url = Url::parse(value).map_err(|_| invalid("invalid CDN issuer URL"))?;
        if url.scheme() != "https"
            || url.host().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(invalid(
                "CDN issuer URLs must be HTTPS without credentials, query or fragment",
            ));
        }
    }
    Ok(())
}

fn invalid(message: &'static str) -> AuthError {
    AuthError::InvalidCrabAuthRequest(message.to_owned())
}
