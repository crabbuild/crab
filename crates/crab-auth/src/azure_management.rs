//! Azure Resource Manager authentication for control-plane reads.

use reqwest::header::HeaderValue;

use crate::error::{AuthError, Result};

/// Acquire a sensitive bearer header for the public Azure Resource Manager audience.
pub async fn management_authorization() -> Result<HeaderValue> {
    // The SDK's credential errors can include identity endpoint responses.
    let credential = azure_identity::create_credential().map_err(|_| AuthError::NoCredentials)?;
    let token = credential
        .get_token(&["https://management.azure.com/.default"])
        .await
        .map_err(|_| AuthError::NoCredentials)?;
    let mut header = HeaderValue::try_from(format!("Bearer {}", token.token.secret()))
        .map_err(|_| AuthError::NoCredentials)?;
    header.set_sensitive(true);
    Ok(header)
}
