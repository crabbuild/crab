use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::api::invalid;
use super::{TransferGrant, TransferMode, TransferTransport};
use crate::cdn::{
    CdnProvider, canonical_https_base, canonical_origin, canonical_prefix, valid_approval_digest,
};
use crate::error::Result;

/// Approved private placement attached to an authenticated repository resolution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ManagedCdnBinding {
    pub provider: CdnProvider,
    /// Credential-free URL of the selected native bucket or container.
    pub origin: String,
    /// Signing region for the selected S3 bucket.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub s3_region: Option<String>,
    pub repository_prefix: String,
    pub base_url: String,
    /// Digest of the operator-approved private deployment receipt.
    pub approval_sha256: String,
}

impl ManagedCdnBinding {
    /// Return the exact transfer mode allowed to use this binding.
    #[must_use]
    pub const fn mode(&self) -> TransferMode {
        match self.provider {
            CdnProvider::Cloudfront => TransferMode::DirectS3,
            CdnProvider::CloudCdn => TransferMode::DirectGcs,
            CdnProvider::FrontDoor => TransferMode::DirectAzure,
        }
    }

    pub(super) fn validate(&self) -> Result<()> {
        let region_matches = match (self.provider, self.s3_region.as_deref()) {
            (CdnProvider::Cloudfront, Some(region)) => commercial_s3_region(region),
            (CdnProvider::Cloudfront, None) | (_, Some(_)) => false,
            (_, None) => true,
        };
        if !region_matches
            || !valid_approval_digest(&self.approval_sha256)
            || !canonical_origin(self.provider, &self.origin)
            || !canonical_prefix(&self.repository_prefix)
            || !canonical_https_base(&self.base_url)
        {
            return invalid("managed repository has an invalid CDN approval");
        }
        Ok(())
    }

    pub(super) fn validate_grant(&self, grant: &TransferGrant) -> Result<()> {
        self.validate()?;
        let TransferTransport::Direct { direct } = &grant.transport else {
            return invalid("approved managed CDN placement requires native direct transfer");
        };
        let (provider, origin) = direct.cdn_origin();
        if grant.cdn_approval_sha256.as_deref() != Some(self.approval_sha256.as_str())
            || direct.endpoint.is_some()
            || provider != self.provider
            || origin != self.origin
            || (provider == CdnProvider::Cloudfront && direct.region != self.s3_region)
            || grant.storage_scope.repository_prefix != self.repository_prefix
            || grant
                .cdn
                .as_ref()
                .is_some_and(|cdn| cdn.base_url != self.base_url)
        {
            return invalid("transfer grant conflicts with the approved managed CDN deployment");
        }
        Ok(())
    }
}

fn commercial_s3_region(region: &str) -> bool {
    let mut parts = region.split('-');
    let letters = |part: &str| part.bytes().all(|byte| byte.is_ascii_lowercase());
    matches!(
        (parts.next(), parts.next(), parts.next(), parts.next()),
        (Some(country), Some(area), Some(index), None)
            if country.len() == 2 && country != "cn" && letters(country)
                && !area.is_empty() && letters(area)
                && index.parse::<u32>().is_ok_and(|number| number > 0)
                && index.bytes().all(|byte| byte.is_ascii_digit()) && !index.starts_with('0')
    )
}
