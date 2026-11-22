use std::{
    fs::File,
    io::{self, Read as _},
    sync::OnceLock,
    time::Instant,
};

use reqwest::{RequestBuilder, Response, header};
use sha2::{Digest as _, Sha256, digest::Output};

use super::{CdnConsumer, CdnPolicy, CdnReadFamily};
use crate::{StorageProviderKind, Store, StoreLayout};

const TARGET: &str = "crab_storage::cdn::transfer";
const GCS_VIEWER_LOG_TARGET: &str = "crab_storage::cdn::gcs_viewer_log";
const PUBLIC_TRANSITION_TARGET: &str = "crab_storage::cdn::public_transition";
const READ_TARGET: &str = "crab_storage::cdn::read_target";
const SERVICE_BUILD_TARGET: &str = "crab_storage::cdn::service_build";
const SERVICE_LFS_CACHE_TARGET: &str = "crab_storage::cdn::service_lfs_cache";
static SERVICE_INSTANCE: OnceLock<String> = OnceLock::new();

/// Identify the storage targets used for redacted CDN qualification evidence.
#[must_use]
pub fn is_read_evidence_target(target: &str) -> bool {
    matches!(
        target,
        TARGET
            | PUBLIC_TRANSITION_TARGET
            | GCS_VIEWER_LOG_TARGET
            | READ_TARGET
            | SERVICE_BUILD_TARGET
            | SERVICE_LFS_CACHE_TARGET
            | crate::native_read_telemetry::TARGET
    )
}

fn service_instance() -> &'static str {
    SERVICE_INSTANCE.get_or_init(|| format!("{:x}", Sha256::digest(rand::random::<[u8; 32]>())))
}

/// Record the cache root captured by an ordinary LFS cache operation.
pub fn observe_service_lfs_cache(
    path: &object_store::path::Path,
    root: impl FnOnce() -> Option<[u8; 32]>,
) {
    if tracing::enabled!(target: SERVICE_LFS_CACHE_TARGET, tracing::Level::DEBUG)
        && let Some(root) = root()
    {
        tracing::debug!(target: SERVICE_LFS_CACHE_TARGET, parent: None,
            service_instance_sha256 = service_instance(),
            lfs_key_sha256 = %format_args!("{:x}", Sha256::digest(path.as_ref().as_bytes())),
            cache_root_sha256 = %format_args!("{:x}", Output::<Sha256>::from(root)),
            "CDN service LFS cache"
        );
    }
}

/// Record the running service binary and its compiled CDN policy for qualification.
pub fn observe_service_build(
    consumer: CdnConsumer,
    source: &str,
    target: &str,
    profile: &str,
    features: &str,
) {
    if !tracing::enabled!(target: SERVICE_BUILD_TARGET, tracing::Level::DEBUG) {
        return;
    }
    let digest = match service_binary_digest() {
        Ok(digest) => digest,
        Err(error) => {
            tracing::warn!(target: SERVICE_BUILD_TARGET, parent: None,
                error_kind = ?error.kind(), "CDN service executable could not be fingerprinted");
            return;
        }
    };
    tracing::debug!(target: SERVICE_BUILD_TARGET, parent: None,
        consumer = consumer.label(),
        service_instance_sha256 = service_instance(),
        source_sha = source,
        target_triple = target,
        cargo_profile = profile,
        cargo_features = features,
        qualification_feature = super::qualification_enabled(),
        release_policy_sha256 = %super::release_policy_sha256(),
        binary_sha256 = %format_args!("{digest:x}"),
        "CDN service build identity"
    );
}

fn service_binary_digest() -> io::Result<Output<Sha256>> {
    // Linux container measurements must hash the running inode, even if a
    // staged executable has replaced its directory entry since startup.
    #[cfg(target_os = "linux")]
    let mut binary = File::open("/proc/self/exe")?;
    #[cfg(not(target_os = "linux"))]
    let mut binary = File::open(std::env::current_exe()?)?;
    let mut digest = Sha256::new();
    let mut chunk = [0; 64 * 1024];
    loop {
        let length = binary.read(&mut chunk)?;
        if length == 0 {
            return Ok(digest.finalize());
        }
        digest.update(&chunk[..length]);
    }
}

impl StoreLayout<Store> {
    /// Record the selected physical repository and attached viewer as fingerprints.
    pub fn observe_cdn_read_target(&self, consumer: CdnConsumer) {
        if !tracing::enabled!(target: READ_TARGET, tracing::Level::DEBUG) {
            return;
        }
        let store = self.store();
        let prefix = self.repo_prefix();
        let Some(origin) = store.native_cdn_origin() else {
            return;
        };
        if store
            .read_prefix_target_identity(&object_store::path::Path::from(prefix))
            .is_none()
        {
            return;
        }
        let binding = store.cdn_binding_sha256();
        let publication = store.cdn_public_export_sha256();
        let mut prefix_hash = Sha256::new();
        prefix_hash.update(prefix.as_bytes());
        prefix_hash.update(b"/");
        tracing::event!(
            target: READ_TARGET,
            parent: None,
            tracing::Level::DEBUG,
            consumer = consumer.label(),
            service_instance_sha256 = service_instance(),
            origin_sha256 = %format_args!("{:x}", Sha256::digest(origin.as_bytes())),
            prefix_sha256 = %format_args!("{:x}", prefix_hash.finalize()),
            binding_sha256 = binding.as_deref().unwrap_or_default(),
            public_export_sha256 = publication.as_deref().unwrap_or_default(),
            "CDN service read target"
        );
    }
}

#[derive(Clone, Copy)]
pub(crate) struct PublicTransitionObservation {
    approval: Output<Sha256>,
    key: Output<Sha256>,
}

impl PublicTransitionObservation {
    pub(crate) fn new(approval: Output<Sha256>, key: &str) -> Option<Self> {
        tracing::enabled!(target: PUBLIC_TRANSITION_TARGET, tracing::Level::DEBUG).then(|| Self {
            approval,
            key: Sha256::digest(key.as_bytes()),
        })
    }

    pub(crate) fn record(
        self,
        policy: CdnPolicy,
        family: CdnReadFamily,
        observation: &'static str,
        oid: Option<&[u8]>,
    ) {
        let oid = oid.map(|oid| format!("{:x}", Sha256::digest(oid)));
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
            git_blob_oid_sha256 = oid.as_deref(),
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

enum PayloadIdentity<'a> {
    None,
    GitBlob(&'a [u8]),
    GitPack(blake3::Hash),
    LfsKey(&'a str),
    XorbKey(&'a str),
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
        self.record_verified_with_blob(None);
    }

    /// Report a verified edge blob and the fingerprint of its exact Git object ID.
    pub fn record_verified_git_blob(self, oid: &[u8]) {
        self.record_verified_with_blob(Some(oid));
    }

    fn record_verified_with_blob(self, oid: Option<&[u8]>) {
        self.record(
            "edge",
            oid.map_or(PayloadIdentity::None, PayloadIdentity::GitBlob),
        );
        if let Some(public) = self.public {
            public.record(self.policy, self.family, "verified_edge", oid);
        }
    }

    /// Report an edge pack after its complete authenticated BLAKE3 digest matches.
    pub fn record_verified_git_pack(self, hash: blake3::Hash) {
        self.record("edge", PayloadIdentity::GitPack(hash));
        if let Some(public) = self.public {
            public.record(self.policy, self.family, "verified_edge", None);
        }
    }

    pub(crate) fn record_verified_lfs(self, key: &str) {
        self.record("edge", PayloadIdentity::LfsKey(key));
        if let Some(public) = self.public {
            public.record(self.policy, self.family, "verified_edge", None);
        }
    }

    pub(crate) fn record_local_lfs(self, key: &str) {
        self.record("local", PayloadIdentity::LfsKey(key));
    }

    pub(crate) fn record_verified_xorb(self, key: &str) {
        self.record("edge", PayloadIdentity::XorbKey(key));
    }

    pub(crate) fn record_local_xorb(self, key: &str) {
        self.record("local", PayloadIdentity::XorbKey(key));
    }

    pub(crate) fn record_origin_xorb(self, key: &str) {
        self.record("origin", PayloadIdentity::XorbKey(key));
    }

    /// Report a verified local payload hit on the selected CDN-enabled route.
    pub fn record_local(self) {
        self.record("local", PayloadIdentity::None);
    }

    pub(crate) fn record_local_git_blob(self, oid: &[u8]) {
        self.record("local", PayloadIdentity::GitBlob(oid));
    }

    fn record(self, source: &'static str, identity: PayloadIdentity<'_>) {
        let message = match self.family {
            CdnReadFamily::Lfs => "CDN verified LFS payload",
            CdnReadFamily::Xorb => "CDN verified xorb payload",
            _ => "CDN verified Git payload",
        };
        let (oid, pack, lfs, xorb) = match identity {
            PayloadIdentity::None => (None, None, None, None),
            PayloadIdentity::GitBlob(oid) => {
                (Some(format!("{:x}", Sha256::digest(oid))), None, None, None)
            }
            PayloadIdentity::GitPack(pack) => (None, Some(pack.to_hex()), None, None),
            PayloadIdentity::LfsKey(key) => {
                (None, None, Some(format!("{:x}", Sha256::digest(key))), None)
            }
            PayloadIdentity::XorbKey(key) => {
                (None, None, None, Some(format!("{:x}", Sha256::digest(key))))
            }
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
            git_blob_oid_sha256 = oid.as_deref(),
            git_pack_blake3 = pack.as_ref().map(|digest| digest.as_str()),
            lfs_key_sha256 = lfs.as_deref(),
            xorb_key_sha256 = xorb.as_deref(),
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

    pub(super) fn prepare_request(&mut self, request: RequestBuilder) -> RequestBuilder {
        if self.policy.provider != StorageProviderKind::Gcs
            || !tracing::enabled!(target: GCS_VIEWER_LOG_TARGET, tracing::Level::TRACE)
        {
            return request;
        }
        // Backend buckets log the viewer User-Agent and the qualified cache
        // policy keeps it out of the cache key. Persist only its fingerprint.
        let user_agent = format!("crab-cdn-qualification/{:032x}", rand::random::<u128>());
        self.request_id = Some(Sha256::digest(user_agent.as_bytes()));
        request.header(header::USER_AGENT, user_agent)
    }

    pub(super) fn response(&mut self, response: &Response) {
        self.status = response.status().as_u16();
        if self.policy.provider != StorageProviderKind::Gcs {
            self.request_id = viewer_request_id(self.policy.provider, response.headers());
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
            .filter(|_| self.status != 0)
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

fn viewer_request_id(
    provider: StorageProviderKind,
    headers: &reqwest::header::HeaderMap,
) -> Option<Output<Sha256>> {
    let header = match provider {
        StorageProviderKind::S3 => "x-amz-cf-id",
        StorageProviderKind::Azure => "x-azure-ref",
        _ => return None,
    };
    let values = headers.get_all(header);
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
    fn read_evidence_excludes_other_storage_and_signed_request_targets() {
        for target in [
            TARGET,
            PUBLIC_TRANSITION_TARGET,
            GCS_VIEWER_LOG_TARGET,
            READ_TARGET,
            SERVICE_BUILD_TARGET,
            SERVICE_LFS_CACHE_TARGET,
            crate::native_read_telemetry::TARGET,
        ] {
            assert!(is_read_evidence_target(target), "{target}");
        }
        for target in [
            "crab_storage::cdn",
            "crab_storage::cdn::transfer::debug",
            "crab_storage::store",
            "s3s::service",
            "reqwest::connect",
        ] {
            assert!(!is_read_evidence_target(target), "{target}");
        }
    }

    #[test]
    fn viewer_request_id_is_hashed_only_for_the_selected_provider_and_one_header() {
        use reqwest::header::{HeaderMap, HeaderValue};

        let mut headers = HeaderMap::new();
        assert_eq!(viewer_request_id(StorageProviderKind::S3, &headers), None,);
        headers.insert("x-amz-cf-id", HeaderValue::from_static("opaque-example=="));
        headers.insert("x-azure-ref", HeaderValue::from_static("opaque-azure-ref"));
        assert_eq!(
            viewer_request_id(StorageProviderKind::S3, &headers),
            Some(Sha256::digest("opaque-example=="))
        );
        assert_eq!(
            viewer_request_id(StorageProviderKind::Azure, &headers),
            Some(Sha256::digest("opaque-azure-ref"))
        );
        assert!(viewer_request_id(StorageProviderKind::Local, &headers).is_none());
        headers.append("x-amz-cf-id", HeaderValue::from_static("other"));
        assert!(viewer_request_id(StorageProviderKind::S3, &headers).is_none());
        headers.insert("x-azure-ref", HeaderValue::from_static("opaque example"));
        assert!(viewer_request_id(StorageProviderKind::Azure, &headers).is_none());
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
