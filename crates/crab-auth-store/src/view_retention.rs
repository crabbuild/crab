//! Bind issuer claims to the common captured-read lifetime.

use crab_auth::crab_auth_client::{ViewRetention, ViewRetentionLease};
use crab_coordination::{
    CoordinationError, Result,
    read_retention_runtime::{ReadRetentionSession, RetainedRead, RetentionOwner},
};

/// Register an issuer claim before capture so shutdown can drain its release.
pub async fn acquire(
    issuer: &ViewRetention,
    session: &ReadRetentionSession,
) -> Result<(String, RetainedRead)> {
    let admission = session.capture()?;
    let (digest, lease) = issuer.acquire().await.map_err(issuer_error)?;
    Ok((digest, admission.retain(IssuerRetention(lease))?))
}

struct IssuerRetention(ViewRetentionLease);

#[async_trait::async_trait]
impl RetentionOwner for IssuerRetention {
    fn path(&self) -> &str {
        self.0.view_prefix()
    }

    fn schedule(&self) -> Result<(tokio::time::Instant, tokio::time::Instant)> {
        Ok(self.0.schedule())
    }

    async fn renew(&mut self) -> Result<()> {
        self.0.renew().await.map_err(issuer_error)
    }

    async fn release(self) -> Result<()> {
        self.0.release().await.map_err(issuer_error)
    }
}

fn issuer_error(error: crab_auth::error::AuthError) -> CoordinationError {
    CoordinationError::ReadRetentionIssuer(Box::new(error))
}
