//! Bind operator Azure management credentials to public CDN verification.

use crab_storage::{Result, StorageError, cdn::AzureManagementAuthorization};
use reqwest::header::HeaderValue;

/// Supply an independent Azure Resource Manager credential for a public export.
pub struct AzureCdnAuthority;

#[async_trait::async_trait]
impl AzureManagementAuthorization for AzureCdnAuthority {
    async fn authorization(&self) -> Result<HeaderValue> {
        crab_auth::azure_management::management_authorization()
            .await
            .map_err(|_| StorageError::AuthFailed {
                path: "Azure Resource Manager credentials for public CDN placement".into(),
            })
    }
}
