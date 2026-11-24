use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::time::Instant;

use super::{CrabAuthProvider, REQUEST_TIMEOUT};
use crate::error::{AuthError, Result};

const PATH: &str = "/v1/views/retention";
const MAX_REPLY_BYTES: usize = 4096;
const MAX_LIFETIME_MS: u64 = 150_000;

/// Read policy that owns a projected-view claim.
#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ViewReadOperation {
    Mount,
    Fetch,
}

impl ViewReadOperation {
    fn as_str(self) -> &'static str {
        match self {
            Self::Mount => "mount",
            Self::Fetch => "fetch",
        }
    }
}

/// Retention authority bound to the login, read policy and native target of a projected grant.
#[derive(Clone)]
pub struct ViewRetention {
    provider: Arc<CrabAuthProvider>,
    binding: Arc<RetainedView>,
}

pub(super) struct RetainedView {
    repo_url: String,
    view_prefix: String,
    target: String,
    read_operation: ViewReadOperation,
    leases: Mutex<Vec<Weak<LeaseIdentity>>>,
}

#[derive(Clone)]
pub(super) struct LeaseIdentity {
    nonce: String,
    incarnation: String,
}

#[derive(Clone, Copy, Serialize)]
pub(super) struct RetainedViewProof<'a> {
    view_prefix: &'a str,
    target: &'a str,
    reader_nonce: &'a str,
    incarnation: &'a str,
}

impl RetainedView {
    pub(super) fn lease_for(&self, repo_url: &str, operation: &str) -> Result<LeaseIdentity> {
        if self.repo_url != repo_url || operation != self.read_operation.as_str() {
            return Err(AuthError::InvalidCrabAuthRequest(
                "projected read grant source or operation changed".into(),
            ));
        }
        let mut leases = self
            .leases
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        leases.retain(|lease| lease.strong_count() != 0);
        // Refresh may race replacement of the catalog. A proof belongs to a
        // currently owned lease and never extends its lifetime.
        leases
            .iter()
            .rev()
            .find_map(Weak::upgrade)
            .as_deref()
            .cloned()
            .ok_or_else(|| {
                AuthError::InvalidCrabAuthRequest(
                    "projected read grant requires an active retained view".into(),
                )
            })
    }

    pub(super) fn proof<'a>(&'a self, identity: &'a LeaseIdentity) -> RetainedViewProof<'a> {
        RetainedViewProof {
            view_prefix: &self.view_prefix,
            target: &self.target,
            reader_nonce: &identity.nonce,
            incarnation: &identity.incarnation,
        }
    }
}

impl RetainedViewProof<'_> {
    pub(super) fn matches(self, scope: Option<&crab_types::storage::StorageScope>) -> bool {
        scope.is_some_and(|scope| scope.repo_prefix == self.view_prefix)
    }
}

impl ViewRetention {
    /// Bind a projected read to the same issuer client that resolved its grant.
    pub fn new(
        provider: Arc<CrabAuthProvider>,
        repo_url: String,
        view_prefix: String,
        target: String,
        read_operation: ViewReadOperation,
    ) -> Result<Self> {
        if provider.cdn_principal().is_none() {
            return Err(AuthError::InvalidCrabAuthRequest(
                "view retention requires a principal-bound read grant".into(),
            ));
        }
        let binding = Arc::new(RetainedView {
            repo_url,
            view_prefix,
            target,
            read_operation,
            leases: Mutex::new(Vec::new()),
        });
        provider
            .retained_view
            .set(Arc::clone(&binding))
            .map_err(|_| {
                AuthError::InvalidCrabAuthRequest("read grant already has a bound view".into())
            })?;
        Ok(Self { provider, binding })
    }

    /// Acquire a new claim and return the exact catalog digest it retains.
    pub async fn acquire(&self) -> Result<(String, ViewRetentionLease)> {
        let mut nonce = String::with_capacity(64);
        for byte in rand::random::<[u8; 32]>() {
            use std::fmt::Write;
            let _ = write!(nonce, "{byte:02x}");
        }
        let (reply, started) = self.request(&nonce, Action::Acquire).await?;
        let Reply::Acquired {
            repo_prefix,
            view_state_digest,
            revision,
            remaining_ms,
        } = reply
        else {
            return Err(invalid_reply());
        };
        if repo_prefix != self.binding.view_prefix || revision.sequence != 0 || !revision.is_valid()
        {
            return Err(invalid_reply());
        }
        let mut lease = ViewRetentionLease {
            authority: self.clone(),
            identity: Arc::new(LeaseIdentity {
                nonce,
                incarnation: revision.incarnation.clone(),
            }),
            revision,
            started,
            lifetime: Duration::ZERO,
        };
        match lifetime(remaining_ms) {
            Ok(lifetime) => lease.lifetime = lifetime,
            Err(error) => {
                // This exact acquisition can be released even if its timing is invalid.
                if let Err(cleanup) = lease.release().await {
                    tracing::warn!(%cleanup, "failed to release rejected view retention");
                }
                return Err(error);
            }
        }
        let mut leases = self
            .binding
            .leases
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        leases.retain(|lease| lease.strong_count() != 0);
        leases.push(Arc::downgrade(&lease.identity));
        Ok((view_state_digest, lease))
    }

    async fn request(&self, nonce: &str, action: Action<'_>) -> Result<(Reply, Instant)> {
        let login = self.provider.load_bound_login()?;
        match self.send(&login.id_token, nonce, action).await {
            Err(AuthError::CrabAuthRejected { status: 401, .. }) => {
                // An authentication rejection occurs before native mutation.
                // Do not replay transport errors or uncertain issuer results.
                let token = self
                    .provider
                    .refresh_bound_id_token(&login.id_token)
                    .await?;
                self.send(&token, nonce, action).await
            }
            result => result,
        }
    }

    async fn send(
        &self,
        id_token: &str,
        nonce: &str,
        operation: Action<'_>,
    ) -> Result<(Reply, Instant)> {
        let endpoint = self.provider.endpoint_for(PATH);
        let body = Request {
            id_token,
            repo_url: &self.binding.repo_url,
            view_prefix: &self.binding.view_prefix,
            target: &self.binding.target,
            reader_nonce: nonce,
            read_operation: self.binding.read_operation,
            operation,
        };
        let request_error = |source: reqwest::Error| AuthError::CrabAuthRequest {
            endpoint: endpoint.clone(),
            operation: "view retention",
            source: source.without_url(),
        };
        // Anchor the server's remaining time before sending. Network and
        // response processing can only shorten local ownership.
        let started = Instant::now();
        let mut response = self
            .provider
            .http
            .post(&endpoint)
            .bearer_auth(id_token)
            .header(reqwest::header::CACHE_CONTROL, "no-store")
            .json(&body)
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await
            .map_err(&request_error)?;
        if response.status() != reqwest::StatusCode::OK {
            return Err(AuthError::CrabAuthRejected {
                status: response.status().as_u16(),
                endpoint,
                operation: "view retention",
                body: "response body withheld".into(),
            });
        }
        let mut reply = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(&request_error)? {
            if reply.len().saturating_add(chunk.len()) > MAX_REPLY_BYTES {
                return Err(invalid_reply());
            }
            reply.extend_from_slice(&chunk);
        }
        let reply = serde_json::from_slice(&reply).map_err(|_| invalid_reply())?;
        Ok((reply, started))
    }
}

/// Owns the last acknowledged issuer revision for one catalog until readers drain.
pub struct ViewRetentionLease {
    authority: ViewRetention,
    identity: Arc<LeaseIdentity>,
    revision: Revision,
    started: Instant,
    lifetime: Duration,
}

impl ViewRetentionLease {
    /// Return the physical projected prefix for local diagnostics.
    pub fn view_prefix(&self) -> &str {
        &self.authority.binding.view_prefix
    }

    /// Return the first renewal time and conservative local deadline.
    pub fn schedule(&self) -> (Instant, Instant) {
        (
            self.started + self.lifetime * 2 / 3,
            self.started + self.lifetime,
        )
    }

    /// Renew one revision without retrying an uncertain native transition.
    pub async fn renew(&mut self) -> Result<()> {
        let (reply, started) = self
            .authority
            .request(
                &self.identity.nonce,
                Action::Renew {
                    revision: &self.revision,
                },
            )
            .await?;
        let Reply::Renewed {
            revision,
            remaining_ms,
        } = reply
        else {
            return Err(invalid_reply());
        };
        if revision.incarnation != self.revision.incarnation
            || self.revision.sequence.checked_add(1) != Some(revision.sequence)
        {
            return Err(invalid_reply());
        }
        let lifetime = lifetime(remaining_ms)?;
        self.revision = revision;
        self.started = started;
        self.lifetime = lifetime;
        Ok(())
    }

    /// Release only the last acknowledged revision after all readers drain.
    pub async fn release(self) -> Result<()> {
        let nonce = self.identity.nonce.clone();
        drop(self.identity);
        let (reply, _) = self
            .authority
            .request(
                &nonce,
                Action::Release {
                    revision: &self.revision,
                },
            )
            .await?;
        if !matches!(reply, Reply::Released) {
            return Err(invalid_reply());
        }
        Ok(())
    }
}

#[derive(Serialize)]
struct Request<'a> {
    id_token: &'a str,
    repo_url: &'a str,
    view_prefix: &'a str,
    target: &'a str,
    reader_nonce: &'a str,
    read_operation: ViewReadOperation,
    operation: Action<'a>,
}

#[derive(Clone, Copy, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum Action<'a> {
    Acquire,
    Renew { revision: &'a Revision },
    Release { revision: &'a Revision },
}

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
enum Reply {
    Acquired {
        repo_prefix: String,
        view_state_digest: String,
        revision: Revision,
        remaining_ms: u64,
    },
    Renewed {
        revision: Revision,
        remaining_ms: u64,
    },
    Released,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Revision {
    incarnation: String,
    sequence: u64,
}

impl Revision {
    fn is_valid(&self) -> bool {
        uuid::Uuid::try_parse(&self.incarnation).is_ok_and(|id| {
            id.get_version_num() == 7
                && id.get_variant() == uuid::Variant::RFC4122
                && id.to_string() == self.incarnation
        })
    }
}

fn lifetime(remaining_ms: u64) -> Result<Duration> {
    if !(1..=MAX_LIFETIME_MS).contains(&remaining_ms) {
        return Err(invalid_reply());
    }
    Ok(Duration::from_millis(remaining_ms))
}

fn invalid_reply() -> AuthError {
    AuthError::InvalidCredentialResponse("invalid view retention acknowledgement".into())
}
