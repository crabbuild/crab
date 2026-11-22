use std::time::Instant;

use reqwest::Response;
use sha2::{Digest as _, Sha256, digest::Output};

use super::{CdnPolicy, CdnReadFamily};
use crate::StorageProviderKind;

const TARGET: &str = "crab_storage::cdn::transfer";
const PUBLIC_TRANSITION_TARGET: &str = "crab_storage::cdn::public_transition";

#[derive(Clone, Copy)]
pub(crate) struct PublicTransitionObservation {
    approval: Output<Sha256>,
    key: Output<Sha256>,
}

impl PublicTransitionObservation {
    pub(crate) fn new(approval: Output<Sha256>, key: &str) -> Option<Self> {
        (super::qualification_enabled()
            && tracing::enabled!(target: PUBLIC_TRANSITION_TARGET, tracing::Level::DEBUG))
        .then(|| Self {
            approval,
            key: Sha256::digest(key.as_bytes()),
        })
    }

    pub(crate) fn record(
        self,
        policy: CdnPolicy,
        family: CdnReadFamily,
        observation: &'static str,
    ) {
        tracing::event!(
            target: PUBLIC_TRANSITION_TARGET,
            parent: None,
            tracing::Level::DEBUG,
            provider = provider_label(policy.provider),
            consumer = policy.consumer.label(),
            access = policy.visibility.label(),
            profile = policy.profile.label(),
            family = family.label(),
            approval_sha256 = %format_args!("{:x}", self.approval),
            key_sha256 = %format_args!("{:x}", self.key),
            observation,
            "CDN public export observation"
        );
    }
}

/// Redacted source of payload bytes awaiting the reader's integrity check.
#[derive(Clone, Copy)]
pub struct PayloadObservation {
    policy: CdnPolicy,
    family: CdnReadFamily,
    public: Option<PublicTransitionObservation>,
}

impl PayloadObservation {
    pub(crate) fn new(
        policy: CdnPolicy,
        family: CdnReadFamily,
        public: Option<PublicTransitionObservation>,
    ) -> Option<Self> {
        (public.is_some() || tracing::enabled!(target: TARGET, tracing::Level::DEBUG)).then_some(
            Self {
                policy,
                family,
                public,
            },
        )
    }

    /// Report only after accepting content that passed its authenticated digest or object ID check.
    pub fn record_verified(self) {
        self.record("edge");
        if let Some(public) = self.public {
            public.record(self.policy, self.family, "verified_edge");
        }
    }

    /// Report a verified local Git pack hit on the selected CDN-enabled payload route.
    pub fn record_local(self) {
        self.record("local");
    }

    fn record(self, source: &'static str) {
        let message = match self.family {
            CdnReadFamily::Lfs => "CDN verified LFS payload",
            _ => "CDN verified Git payload",
        };
        tracing::event!(
            target: TARGET,
            parent: None,
            tracing::Level::DEBUG,
            provider = provider_label(self.policy.provider),
            consumer = self.policy.consumer.label(),
            access = self.policy.visibility.label(),
            profile = self.policy.profile.label(),
            family = self.family.label(),
            source,
            "{message}"
        );
    }
}

pub(super) struct Transfer {
    policy: CdnPolicy,
    family: CdnReadFamily,
    started: Instant,
    size: u64,
    range: bool,
    status: u16,
    cache: &'static str,
    request_id: Option<Output<Sha256>>,
    bytes: u64,
    pub(super) complete: bool,
}

impl Transfer {
    pub(super) fn start(
        (policy, family): (CdnPolicy, CdnReadFamily),
        size: u64,
        range: bool,
    ) -> Option<Self> {
        tracing::enabled!(target: TARGET, tracing::Level::DEBUG).then(|| Self {
            policy,
            family,
            started: Instant::now(),
            size,
            range,
            status: 0,
            cache: "unknown",
            request_id: None,
            bytes: 0,
            complete: false,
        })
    }

    pub(super) fn response(&mut self, response: &Response) {
        self.status = response.status().as_u16();
        if self.policy.provider == StorageProviderKind::S3 {
            self.request_id = cloudfront_request_id(response.headers());
        }
        let header = if self.policy.provider == StorageProviderKind::Gcs {
            "x-crab-cdn-cache"
        } else {
            "x-cache"
        };
        let values = response.headers().get_all(header);
        let mut values = values.iter();
        if let Some(value) = values.next()
            && values.next().is_none()
            && let Ok(value) = value.to_str()
        {
            self.cache = cache_status(self.policy.provider, self.size, value);
        }
    }

    pub(super) fn received(&mut self, bytes: u64) {
        self.bytes = self.bytes.saturating_add(bytes);
    }
}

impl Drop for Transfer {
    fn drop(&mut self) {
        let request_sha256 = self
            .request_id
            .map(|digest| format!("{digest:x}"))
            .unwrap_or_default();
        // This event can be retained independently of the command's spans.
        tracing::event!(
            target: TARGET,
            parent: None,
            tracing::Level::DEBUG,
            provider = provider_label(self.policy.provider),
            consumer = self.policy.consumer.label(),
            access = self.policy.visibility.label(),
            profile = self.policy.profile.label(),
            family = self.family.label(),
            status = self.status,
            request_sha256 = request_sha256.as_str(),
            cache = self.cache,
            range = self.range,
            bytes = self.bytes,
            complete = self.complete,
            elapsed_ms = u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX),
            "CDN immutable GET"
        );
    }
}

fn cloudfront_request_id(headers: &reqwest::header::HeaderMap) -> Option<Output<Sha256>> {
    let values = headers.get_all("x-amz-cf-id");
    let mut values = values.iter();
    let value = values.next()?.as_bytes();
    (values.next().is_none()
        && (1..=256).contains(&value.len())
        && value.iter().all(u8::is_ascii_graphic))
    .then(|| Sha256::digest(value))
}

fn provider_label(provider: StorageProviderKind) -> &'static str {
    match provider {
        StorageProviderKind::S3 => "s3",
        StorageProviderKind::Gcs => "gcs",
        StorageProviderKind::Azure => "azure",
        StorageProviderKind::Local => "local",
    }
}

fn cache_status(provider: StorageProviderKind, object_size: u64, value: &str) -> &'static str {
    // Front Door's header describes only the first chunk, documented as 8 MB.
    if provider == StorageProviderKind::Azure && object_size > 8_000_000 {
        return "unknown";
    }
    match (provider, value) {
        (StorageProviderKind::S3, "Hit from cloudfront") | (StorageProviderKind::Gcs, "hit") => {
            "hit"
        }
        (StorageProviderKind::S3, "Miss from cloudfront") | (StorageProviderKind::Gcs, "miss") => {
            "miss"
        }
        (StorageProviderKind::Azure, "TCP_HIT" | "TCP_REMOTE_HIT") => "hit",
        (StorageProviderKind::Azure, "TCP_MISS") => "miss",
        _ => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cloudfront_request_id_is_hashed_only_when_present_and_unambiguous() {
        use reqwest::header::{HeaderMap, HeaderValue};

        let mut headers = HeaderMap::new();
        assert!(cloudfront_request_id(&headers).is_none());
        headers.insert("x-amz-cf-id", HeaderValue::from_static("opaque-example=="));
        assert_eq!(
            cloudfront_request_id(&headers),
            Some(Sha256::digest("opaque-example=="))
        );
        headers.append("x-amz-cf-id", HeaderValue::from_static("other"));
        assert!(cloudfront_request_id(&headers).is_none());
        headers.insert("x-amz-cf-id", HeaderValue::from_static("opaque example"));
        assert!(cloudfront_request_id(&headers).is_none());
    }

    #[test]
    fn cache_status_is_specific_to_the_provider_and_complete_representation() {
        assert_eq!(
            cache_status(StorageProviderKind::S3, 6, "Hit from cloudfront"),
            "hit"
        );
        assert_eq!(cache_status(StorageProviderKind::S3, 6, "hit"), "unknown");
        assert_eq!(
            cache_status(StorageProviderKind::Gcs, 6, "revalidated"),
            "unknown"
        );
        assert_eq!(cache_status(StorageProviderKind::Gcs, 6, "miss"), "miss");
        assert_eq!(
            cache_status(StorageProviderKind::Azure, 6, "TCP_REMOTE_HIT"),
            "hit"
        );
        assert_eq!(
            cache_status(StorageProviderKind::Azure, 8_000_001, "TCP_HIT"),
            "unknown"
        );
    }
}
