use std::time::Instant;

use reqwest::Response;

use super::{CdnPolicy, CdnReadFamily};
use crate::StorageProviderKind;

const TARGET: &str = "crab_storage::cdn::transfer";

/// Redacted source of Git payload bytes awaiting the reader's integrity check.
#[derive(Clone, Copy)]
pub struct GitPayloadObservation {
    policy: CdnPolicy,
    family: CdnReadFamily,
}

impl GitPayloadObservation {
    pub(crate) fn new(policy: CdnPolicy, family: CdnReadFamily) -> Option<Self> {
        tracing::enabled!(target: TARGET, tracing::Level::DEBUG).then_some(Self { policy, family })
    }

    /// Report only after accepting Git content that passed its authenticated digest or object ID check.
    pub fn record_verified(self) {
        self.record("edge");
    }

    /// Report a verified local Git pack hit on the selected CDN-enabled payload route.
    pub fn record_local(self) {
        self.record("local");
    }

    fn record(self, source: &'static str) {
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
            "CDN verified Git payload"
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
            bytes: 0,
            complete: false,
        })
    }

    pub(super) fn response(&mut self, response: &Response) {
        self.status = response.status().as_u16();
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
            cache = self.cache,
            range = self.range,
            bytes = self.bytes,
            complete = self.complete,
            elapsed_ms = u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX),
            "CDN immutable GET"
        );
    }
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
