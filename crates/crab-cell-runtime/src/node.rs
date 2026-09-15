use bytes::Bytes;
use crab_storage::{CellStorageLayout, ETag, StorageError, map_object_store_error};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};

use crate::{Digest, Error, Result, SessionId};

const MAX_NODE_BYTES: u64 = 64 * 1024;
const MAX_ENDPOINT_BYTES: usize = 512;
const MAX_MODULES: usize = 128;
const MAX_PEER_VERSIONS: usize = 16;
const MAX_ADVERTISEMENT_LIFETIME_MS: i64 = 15_000;
const MAX_CLOCK_SKEW_MS: i64 = 5 * 60_000;
const STALE_ADVERTISEMENT_RETENTION_MS: i64 = MAX_CLOCK_SKEW_MS + MAX_ADVERTISEMENT_LIFETIME_MS;
const MAX_STALE_COLLECTION_ITEMS: usize = 1_024;
const SIGNING_DOMAIN: &[u8] = b"crab.node.v1\0";

/// Capacity hints published by one node boot session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NodeCapacity {
    pub free_memory_bytes: u64,
    pub free_disk_bytes: u64,
    pub job_credits: u32,
}

/// Signed, short-lived identity and capacity advertisement for one node session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeAdvertisement {
    session: SessionId,
    endpoint: String,
    fleet: Digest,
    certificate: Digest,
    image: Digest,
    release: Digest,
    public_key: [u8; 32],
    progress: u64,
    issued_at_ms: i64,
    expires_at_ms: i64,
    module_digests: Vec<Digest>,
    peer_versions: Vec<u32>,
    capacity: NodeCapacity,
    signature: [u8; 64],
}

impl NodeAdvertisement {
    /// Builds and signs a canonical advertisement with the boot-session key.
    #[expect(
        clippy::too_many_arguments,
        reason = "all signed identity fields stay explicit"
    )]
    pub fn sign(
        session: SessionId,
        endpoint: String,
        fleet: Digest,
        certificate: Digest,
        image: Digest,
        release: Digest,
        signing_key: &SigningKey,
        progress: u64,
        issued_at_ms: i64,
        expires_at_ms: i64,
        module_digests: Vec<Digest>,
        peer_versions: Vec<u32>,
        capacity: NodeCapacity,
    ) -> Result<Self> {
        let mut advertisement = Self {
            session,
            endpoint,
            fleet,
            certificate,
            image,
            release,
            public_key: signing_key.verifying_key().to_bytes(),
            progress,
            issued_at_ms,
            expires_at_ms,
            module_digests,
            peer_versions,
            capacity,
            signature: [0; 64],
        };
        advertisement.validate_shape()?;
        advertisement.signature = signing_key.sign(&advertisement.signing_bytes()?).to_bytes();
        Ok(advertisement)
    }

    #[must_use]
    pub const fn session(&self) -> SessionId {
        self.session
    }

    #[must_use]
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    #[must_use]
    pub const fn fleet(&self) -> Digest {
        self.fleet
    }

    #[must_use]
    pub const fn certificate(&self) -> Digest {
        self.certificate
    }

    #[must_use]
    pub const fn image(&self) -> Digest {
        self.image
    }

    #[must_use]
    pub const fn release(&self) -> Digest {
        self.release
    }

    pub fn verifying_key(&self) -> Result<VerifyingKey> {
        VerifyingKey::from_bytes(&self.public_key).map_err(Error::PeerSignature)
    }

    #[must_use]
    pub const fn progress(&self) -> u64 {
        self.progress
    }

    #[must_use]
    pub const fn expires_at_ms(&self) -> i64 {
        self.expires_at_ms
    }

    #[must_use]
    pub fn module_digests(&self) -> &[Digest] {
        &self.module_digests
    }

    #[must_use]
    pub fn peer_versions(&self) -> &[u32] {
        &self.peer_versions
    }

    #[must_use]
    pub const fn capacity(&self) -> NodeCapacity {
        self.capacity
    }

    fn encode(&self) -> Result<Vec<u8>> {
        self.validate_shape()?;
        self.verify_signature()?;
        let encoded = serde_json::to_vec(&RawAdvertisement::from(self))?;
        if encoded.len() as u64 > MAX_NODE_BYTES {
            return Err(Error::Node("advertisement exceeds 64 KiB"));
        }
        Ok(encoded)
    }

    fn decode_canonical(bytes: &[u8]) -> Result<Self> {
        if bytes.len() as u64 > MAX_NODE_BYTES {
            return Err(Error::Node("advertisement exceeds 64 KiB"));
        }
        let raw: RawAdvertisement = serde_json::from_slice(bytes)?;
        let advertisement = Self::try_from(raw)?;
        advertisement.validate_shape()?;
        advertisement.verify_signature()?;
        if advertisement.encode()?.as_slice() != bytes {
            return Err(Error::Node("advertisement JSON is not canonical"));
        }
        Ok(advertisement)
    }

    fn validate_at(&self, now_ms: i64) -> Result<()> {
        self.validate_shape()?;
        if self.issued_at_ms > now_ms.saturating_add(MAX_CLOCK_SKEW_MS)
            || self.expires_at_ms <= now_ms
        {
            return Err(Error::Node("advertisement is not currently valid"));
        }
        Ok(())
    }

    fn validate_shape(&self) -> Result<()> {
        if self.session.as_bytes().iter().all(|byte| *byte == 0)
            || self.fleet.as_bytes().iter().all(|byte| *byte == 0)
            || self.certificate.as_bytes().iter().all(|byte| *byte == 0)
            || self.image.as_bytes().iter().all(|byte| *byte == 0)
            || self.release.as_bytes().iter().all(|byte| *byte == 0)
            || self.public_key.iter().all(|byte| *byte == 0)
        {
            return Err(Error::Node("advertisement identity is zero"));
        }
        if !valid_endpoint(&self.endpoint) {
            return Err(Error::Node("advertisement endpoint is invalid"));
        }
        if self.progress == 0
            || self.issued_at_ms < 0
            || self.expires_at_ms <= self.issued_at_ms
            || self.expires_at_ms.saturating_sub(self.issued_at_ms) > MAX_ADVERTISEMENT_LIFETIME_MS
        {
            return Err(Error::Node("advertisement progress or time is invalid"));
        }
        if self.module_digests.is_empty()
            || self.module_digests.len() > MAX_MODULES
            || !self
                .module_digests
                .windows(2)
                .all(|pair| pair[0].as_bytes() < pair[1].as_bytes())
            || self.peer_versions.is_empty()
            || self.peer_versions.len() > MAX_PEER_VERSIONS
            || !strictly_sorted(&self.peer_versions)
            || self.peer_versions.iter().any(|version| *version != 1)
        {
            return Err(Error::Node("advertisement inventory is invalid"));
        }
        Ok(())
    }

    fn verify_signature(&self) -> Result<()> {
        self.verifying_key()?
            .verify(
                &self.signing_bytes()?,
                &Signature::from_bytes(&self.signature),
            )
            .map_err(Error::PeerSignature)
    }

    fn signing_bytes(&self) -> Result<Vec<u8>> {
        let unsigned = serde_json::to_vec(&RawUnsignedAdvertisement::from(self))?;
        let mut bytes = Vec::with_capacity(SIGNING_DOMAIN.len() + unsigned.len());
        bytes.extend_from_slice(SIGNING_DOMAIN);
        bytes.extend_from_slice(&unsigned);
        Ok(bytes)
    }
}

/// Exact node advertisement plus the token required for conditional refresh.
pub struct VersionedNodeAdvertisement {
    advertisement: NodeAdvertisement,
    token: ETag,
}

impl VersionedNodeAdvertisement {
    #[must_use]
    pub const fn advertisement(&self) -> &NodeAdvertisement {
        &self.advertisement
    }
}

/// Object-store directory for one fleet and compiled release.
#[derive(Clone)]
pub struct NodeDirectory {
    layout: CellStorageLayout,
    fleet: Digest,
    image: Digest,
    release: Digest,
}

impl NodeDirectory {
    #[must_use]
    pub fn new(layout: CellStorageLayout, fleet: Digest, image: Digest, release: Digest) -> Self {
        Self {
            layout,
            fleet,
            image,
            release,
        }
    }

    #[must_use]
    pub const fn fleet(&self) -> Digest {
        self.fleet
    }

    /// Strict-creates one signed boot-session advertisement.
    pub async fn create(
        &self,
        advertisement: NodeAdvertisement,
        now_ms: i64,
    ) -> Result<VersionedNodeAdvertisement> {
        self.validate(&advertisement, now_ms)?;
        let encoded = advertisement.encode()?;
        let path = self.layout.node_path(advertisement.session.as_bytes());
        match self
            .layout
            .store()
            .create_strict_with_etag(&path, Bytes::from(encoded))
            .await
        {
            Ok(token) => Ok(VersionedNodeAdvertisement {
                advertisement,
                token,
            }),
            Err(create_error) => match self.load(advertisement.session, now_ms).await? {
                Some(current) if current.advertisement == advertisement => Ok(current),
                Some(_) | None => Err(create_error.into()),
            },
        }
    }

    /// Loads and verifies one exact, currently valid boot session.
    pub async fn load(
        &self,
        session: SessionId,
        now_ms: i64,
    ) -> Result<Option<VersionedNodeAdvertisement>> {
        let Some((advertisement, token)) = self.load_canonical(session).await? else {
            return Ok(None);
        };
        self.validate(&advertisement, now_ms)?;
        Ok(Some(VersionedNodeAdvertisement {
            advertisement,
            token,
        }))
    }

    /// Reports whether an exact canonical session is currently live.
    ///
    /// Missing and expired sessions return `false`. Malformed, misplaced, or
    /// foreign records fail closed instead of being treated as takeover evidence.
    pub async fn is_live(&self, session: SessionId, now_ms: i64) -> Result<bool> {
        let Some((advertisement, _)) = self.load_canonical(session).await? else {
            return Ok(false);
        };
        self.validate_scope(&advertisement)?;
        if advertisement.issued_at_ms > now_ms.saturating_add(MAX_CLOCK_SKEW_MS) {
            return Err(Error::Node("advertisement is not currently valid"));
        }
        Ok(advertisement.expires_at_ms > now_ms)
    }

    /// Streams and verifies every currently live boot-session advertisement.
    ///
    /// Expired records do not count against `limit`; malformed, misplaced, or
    /// foreign live records fail closed so maintenance cannot mistake an active
    /// incompatible fleet for an offline deployment.
    pub async fn live(&self, now_ms: i64, limit: usize) -> Result<Vec<NodeAdvertisement>> {
        if limit == 0 {
            return Err(Error::Node("live node limit must be nonzero"));
        }
        let prefix = self.layout.node_directory_path();
        let mut stream = self.layout.store().inner().list(Some(&prefix));
        let mut live = Vec::new();
        while let Some(item) = stream.next().await {
            let meta = item.map_err(|error| map_object_store_error(error, prefix.as_ref()))?;
            let (body, _) = match self
                .layout
                .store()
                .get_with_etag_bounded(&meta.location, MAX_NODE_BYTES)
                .await
            {
                Ok(value) => value,
                Err(StorageError::NotFound { .. }) => continue,
                Err(error) => return Err(error.into()),
            };
            let NodeRecord::Advertisement(advertisement) = NodeRecord::decode_canonical(&body)?
            else {
                continue;
            };
            validate_record_path(&self.layout, advertisement.session, &meta.location)?;
            if advertisement.expires_at_ms <= now_ms {
                continue;
            }
            self.validate(&advertisement, now_ms)?;
            if live.len() == limit {
                return Err(Error::Node("live node directory exceeds its limit"));
            }
            live.push(*advertisement);
        }
        live.sort_unstable_by(|left, right| left.session.as_bytes().cmp(right.session.as_bytes()));
        Ok(live)
    }

    /// Fences and removes a bounded number of advertisements past the clock-skew horizon.
    pub async fn collect_stale(&self, now_ms: i64, limit: usize) -> Result<usize> {
        if now_ms < 0 || !(1..=MAX_STALE_COLLECTION_ITEMS).contains(&limit) {
            return Err(Error::Node(
                "stale node collection limit or time is invalid",
            ));
        }
        let cutoff_ms = now_ms.saturating_sub(STALE_ADVERTISEMENT_RETENTION_MS);
        let prefix = self.layout.node_directory_path();
        let mut stream = self.layout.store().inner().list(Some(&prefix));
        let mut removed = 0;
        while removed < limit
            && let Some(item) = stream.next().await
        {
            let meta = item.map_err(|error| map_object_store_error(error, prefix.as_ref()))?;
            let Some((record, token)) = self.load_record_at(&meta.location).await? else {
                continue;
            };
            let session = record.session();
            validate_record_path(&self.layout, session, &meta.location)?;
            match record {
                NodeRecord::Tombstone(_) => {
                    self.delete_collected(&meta.location).await?;
                    removed += 1;
                }
                NodeRecord::Advertisement(advertisement)
                    if advertisement.expires_at_ms <= cutoff_ms =>
                {
                    let tombstone = NodeTombstone::new(
                        advertisement.session,
                        advertisement.expires_at_ms,
                        now_ms,
                    )?;
                    let encoded = tombstone.encode()?;
                    match self
                        .layout
                        .store()
                        .update(&meta.location, Bytes::from(encoded), token)
                        .await
                    {
                        Ok(_) => {
                            self.delete_collected(&meta.location).await?;
                            removed += 1;
                        }
                        Err(update_error) => match self.load_record_at(&meta.location).await? {
                            None => removed += 1,
                            Some((NodeRecord::Tombstone(_), _)) => {
                                self.delete_collected(&meta.location).await?;
                                removed += 1;
                            }
                            Some((NodeRecord::Advertisement(_), _)) => {
                                if !matches!(update_error, StorageError::StateConflict { .. }) {
                                    return Err(update_error.into());
                                }
                            }
                        },
                    }
                }
                NodeRecord::Advertisement(_) => {}
            }
        }
        Ok(removed)
    }

    /// Authenticates one request against its live advertisement and mTLS leaf digest.
    pub async fn verify_peer_request(
        &self,
        input: &[u8],
        certificate: Digest,
        certificate_public_key: [u8; 32],
        now_ms: i64,
    ) -> Result<crate::VerifiedPeerRequest> {
        let session = crate::claimed_peer_session(input)?;
        let enrolled = self
            .load(session, now_ms)
            .await?
            .ok_or(Error::PeerAuthorization("peer session is not enrolled"))?;
        if enrolled.advertisement.certificate != certificate {
            return Err(Error::PeerAuthorization(
                "mTLS certificate does not match peer session",
            ));
        }
        if enrolled.advertisement.public_key != certificate_public_key {
            return Err(Error::PeerAuthorization(
                "mTLS certificate key does not match peer session",
            ));
        }
        crate::PeerVerifier::new(
            session,
            self.release,
            enrolled.advertisement.verifying_key()?,
        )
        .verify(input, now_ms)
    }

    /// Conditionally publishes the next heartbeat for the same boot session.
    pub async fn refresh(
        &self,
        observed: &VersionedNodeAdvertisement,
        next: NodeAdvertisement,
        now_ms: i64,
    ) -> Result<VersionedNodeAdvertisement> {
        self.validate(&next, now_ms)?;
        validate_successor(&observed.advertisement, &next)?;
        let path = self.layout.node_path(next.session.as_bytes());
        match self
            .layout
            .store()
            .update(&path, Bytes::from(next.encode()?), observed.token.clone())
            .await
        {
            Ok(token) => Ok(VersionedNodeAdvertisement {
                advertisement: next,
                token,
            }),
            Err(update_error) => match self.load(next.session, now_ms).await? {
                Some(current) if current.advertisement == next => Ok(current),
                Some(_) | None => Err(update_error.into()),
            },
        }
    }

    fn validate(&self, advertisement: &NodeAdvertisement, now_ms: i64) -> Result<()> {
        advertisement.validate_at(now_ms)?;
        advertisement.verify_signature()?;
        self.validate_scope(advertisement)
    }

    async fn load_canonical(
        &self,
        session: SessionId,
    ) -> Result<Option<(NodeAdvertisement, ETag)>> {
        let path = self.layout.node_path(session.as_bytes());
        let Some((record, token)) = self.load_record_at(&path).await? else {
            return Ok(None);
        };
        if record.session() != session {
            return Err(Error::Node("advertisement path and session differ"));
        }
        match record {
            NodeRecord::Advertisement(advertisement) => Ok(Some((*advertisement, token))),
            NodeRecord::Tombstone(_) => Ok(None),
        }
    }

    async fn load_record_at(
        &self,
        path: &object_store::path::Path,
    ) -> Result<Option<(NodeRecord, ETag)>> {
        let (body, token) = match self
            .layout
            .store()
            .get_with_etag_bounded(path, MAX_NODE_BYTES)
            .await
        {
            Ok(value) => value,
            Err(StorageError::NotFound { .. }) => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        Ok(Some((NodeRecord::decode_canonical(&body)?, token)))
    }

    async fn delete_collected(&self, path: &object_store::path::Path) -> Result<()> {
        match self.layout.store().delete(path).await {
            Ok(()) | Err(StorageError::NotFound { .. }) => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    fn validate_scope(&self, advertisement: &NodeAdvertisement) -> Result<()> {
        if advertisement.fleet != self.fleet
            || advertisement.image != self.image
            || advertisement.release != self.release
        {
            return Err(Error::Node("advertisement fleet, image or release differs"));
        }
        Ok(())
    }
}

fn validate_record_path(
    layout: &CellStorageLayout,
    session: SessionId,
    path: &object_store::path::Path,
) -> Result<()> {
    if layout.node_path(session.as_bytes()) != *path {
        return Err(Error::Node("advertisement path and session differ"));
    }
    Ok(())
}

enum NodeRecord {
    Advertisement(Box<NodeAdvertisement>),
    Tombstone(NodeTombstone),
}

impl NodeRecord {
    fn decode_canonical(bytes: &[u8]) -> Result<Self> {
        if let Ok(advertisement) = NodeAdvertisement::decode_canonical(bytes) {
            return Ok(Self::Advertisement(Box::new(advertisement)));
        }
        Ok(Self::Tombstone(NodeTombstone::decode_canonical(bytes)?))
    }

    const fn session(&self) -> SessionId {
        match self {
            Self::Advertisement(advertisement) => advertisement.session,
            Self::Tombstone(tombstone) => tombstone.session,
        }
    }
}

struct NodeTombstone {
    session: SessionId,
    expires_at_ms: i64,
    collected_at_ms: i64,
}

impl NodeTombstone {
    fn new(session: SessionId, expires_at_ms: i64, collected_at_ms: i64) -> Result<Self> {
        let tombstone = Self {
            session,
            expires_at_ms,
            collected_at_ms,
        };
        tombstone.validate()?;
        Ok(tombstone)
    }

    fn encode(&self) -> Result<Vec<u8>> {
        self.validate()?;
        let encoded = serde_json::to_vec(&RawNodeTombstoneEnvelope::from(self))?;
        if encoded.len() as u64 > MAX_NODE_BYTES {
            return Err(Error::Node("node tombstone exceeds 64 KiB"));
        }
        Ok(encoded)
    }

    fn decode_canonical(bytes: &[u8]) -> Result<Self> {
        if bytes.len() as u64 > MAX_NODE_BYTES {
            return Err(Error::Node("node tombstone exceeds 64 KiB"));
        }
        let raw: RawNodeTombstoneEnvelope = serde_json::from_slice(bytes)?;
        if raw.tombstone.version != 1 {
            return Err(Error::Node("unsupported node tombstone version"));
        }
        let tombstone = Self {
            session: SessionId::from_bytes(decode_hex(&raw.tombstone.session)?),
            expires_at_ms: canonical_i64(&raw.tombstone.expires_at_ms)?,
            collected_at_ms: canonical_i64(&raw.tombstone.collected_at_ms)?,
        };
        tombstone.validate()?;
        if tombstone.encode()?.as_slice() != bytes {
            return Err(Error::Node("node tombstone JSON is not canonical"));
        }
        Ok(tombstone)
    }

    fn validate(&self) -> Result<()> {
        if self.session.as_bytes().iter().all(|byte| *byte == 0)
            || self.expires_at_ms < 0
            || self.collected_at_ms
                < self
                    .expires_at_ms
                    .saturating_add(STALE_ADVERTISEMENT_RETENTION_MS)
        {
            return Err(Error::Node("node tombstone is invalid"));
        }
        Ok(())
    }
}

fn validate_successor(current: &NodeAdvertisement, next: &NodeAdvertisement) -> Result<()> {
    if current.session != next.session
        || current.endpoint != next.endpoint
        || current.fleet != next.fleet
        || current.certificate != next.certificate
        || current.image != next.image
        || current.release != next.release
        || current.public_key != next.public_key
        || current.module_digests != next.module_digests
        || current.peer_versions != next.peer_versions
        || next.progress < current.progress
        || next.issued_at_ms <= current.issued_at_ms
        || next.expires_at_ms <= current.expires_at_ms
    {
        return Err(Error::Node(
            "advertisement refresh changed boot identity or regressed",
        ));
    }
    Ok(())
}

fn valid_endpoint(endpoint: &str) -> bool {
    if endpoint.len() > MAX_ENDPOINT_BYTES
        || !endpoint.is_ascii()
        || endpoint
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte == b' ')
        || endpoint.contains(['@', '?', '#'])
    {
        return false;
    }
    let Some(authority) = endpoint.strip_prefix("https://") else {
        return false;
    };
    let authority = authority.strip_suffix('/').unwrap_or(authority);
    if authority.is_empty() || authority.contains('/') {
        return false;
    }
    let (host, port) = match authority.strip_prefix('[') {
        Some(bracketed) => {
            let Some((host, suffix)) = bracketed.split_once(']') else {
                return false;
            };
            if host.parse::<std::net::Ipv6Addr>().is_err() {
                return false;
            }
            let port = if suffix.is_empty() {
                None
            } else {
                let Some(port) = suffix.strip_prefix(':') else {
                    return false;
                };
                Some(port)
            };
            (None, port)
        }
        None => match authority.rsplit_once(':') {
            Some((host, port)) => (Some(host), Some(port)),
            None => (Some(authority), None),
        },
    };
    if host.is_some_and(|host| !valid_host(host)) {
        return false;
    }
    port.is_none_or(|port| {
        port.parse::<u16>()
            .is_ok_and(|parsed| parsed != 0 && parsed.to_string() == port)
    })
}

fn valid_host(host: &str) -> bool {
    if host.parse::<std::net::Ipv4Addr>().is_ok() {
        return true;
    }
    !host.is_empty()
        && host.len() <= 253
        && host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
                && label
                    .as_bytes()
                    .first()
                    .is_some_and(u8::is_ascii_alphanumeric)
                && label
                    .as_bytes()
                    .last()
                    .is_some_and(u8::is_ascii_alphanumeric)
        })
}

fn strictly_sorted<T: Ord>(values: &[T]) -> bool {
    values.windows(2).all(|pair| pair[0] < pair[1])
}

#[derive(Serialize)]
struct RawUnsignedAdvertisement {
    version: u8,
    session: String,
    endpoint: String,
    fleet: String,
    certificate: String,
    image: String,
    release: String,
    public_key: String,
    progress: String,
    issued_at_ms: String,
    expires_at_ms: String,
    module_digests: Vec<String>,
    peer_versions: Vec<u32>,
    free_memory_bytes: String,
    free_disk_bytes: String,
    job_credits: u32,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RawNodeTombstoneEnvelope {
    tombstone: RawNodeTombstone,
}

impl From<&NodeTombstone> for RawNodeTombstoneEnvelope {
    fn from(value: &NodeTombstone) -> Self {
        Self {
            tombstone: RawNodeTombstone {
                version: 1,
                session: encode_hex(value.session.as_bytes()),
                expires_at_ms: value.expires_at_ms.to_string(),
                collected_at_ms: value.collected_at_ms.to_string(),
            },
        }
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RawNodeTombstone {
    version: u8,
    session: String,
    expires_at_ms: String,
    collected_at_ms: String,
}

impl From<&NodeAdvertisement> for RawUnsignedAdvertisement {
    fn from(value: &NodeAdvertisement) -> Self {
        Self {
            version: 1,
            session: encode_hex(value.session.as_bytes()),
            endpoint: value.endpoint.clone(),
            fleet: encode_hex(value.fleet.as_bytes()),
            certificate: encode_hex(value.certificate.as_bytes()),
            image: encode_hex(value.image.as_bytes()),
            release: encode_hex(value.release.as_bytes()),
            public_key: encode_hex(&value.public_key),
            progress: value.progress.to_string(),
            issued_at_ms: value.issued_at_ms.to_string(),
            expires_at_ms: value.expires_at_ms.to_string(),
            module_digests: value
                .module_digests
                .iter()
                .map(|digest| encode_hex(digest.as_bytes()))
                .collect(),
            peer_versions: value.peer_versions.clone(),
            free_memory_bytes: value.capacity.free_memory_bytes.to_string(),
            free_disk_bytes: value.capacity.free_disk_bytes.to_string(),
            job_credits: value.capacity.job_credits,
        }
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RawAdvertisement {
    #[serde(flatten)]
    unsigned: RawDecodedUnsignedAdvertisement,
    signature: String,
}

impl From<&NodeAdvertisement> for RawAdvertisement {
    fn from(value: &NodeAdvertisement) -> Self {
        Self {
            unsigned: RawDecodedUnsignedAdvertisement::from(value),
            signature: encode_hex(&value.signature),
        }
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RawDecodedUnsignedAdvertisement {
    version: u8,
    session: String,
    endpoint: String,
    fleet: String,
    certificate: String,
    image: String,
    release: String,
    public_key: String,
    progress: String,
    issued_at_ms: String,
    expires_at_ms: String,
    module_digests: Vec<String>,
    peer_versions: Vec<u32>,
    free_memory_bytes: String,
    free_disk_bytes: String,
    job_credits: u32,
}

impl From<&NodeAdvertisement> for RawDecodedUnsignedAdvertisement {
    fn from(value: &NodeAdvertisement) -> Self {
        let raw = RawUnsignedAdvertisement::from(value);
        Self {
            version: raw.version,
            session: raw.session,
            endpoint: raw.endpoint,
            fleet: raw.fleet,
            certificate: raw.certificate,
            image: raw.image,
            release: raw.release,
            public_key: raw.public_key,
            progress: raw.progress,
            issued_at_ms: raw.issued_at_ms,
            expires_at_ms: raw.expires_at_ms,
            module_digests: raw.module_digests,
            peer_versions: raw.peer_versions,
            free_memory_bytes: raw.free_memory_bytes,
            free_disk_bytes: raw.free_disk_bytes,
            job_credits: raw.job_credits,
        }
    }
}

impl TryFrom<RawAdvertisement> for NodeAdvertisement {
    type Error = Error;

    fn try_from(value: RawAdvertisement) -> Result<Self> {
        let raw = value.unsigned;
        if raw.version != 1 {
            return Err(Error::Node("unsupported advertisement version"));
        }
        Ok(Self {
            session: SessionId::from_bytes(decode_hex(&raw.session)?),
            endpoint: raw.endpoint,
            fleet: Digest::from_bytes(decode_hex(&raw.fleet)?),
            certificate: Digest::from_bytes(decode_hex(&raw.certificate)?),
            image: Digest::from_bytes(decode_hex(&raw.image)?),
            release: Digest::from_bytes(decode_hex(&raw.release)?),
            public_key: decode_hex(&raw.public_key)?,
            progress: canonical_u64(&raw.progress)?,
            issued_at_ms: canonical_i64(&raw.issued_at_ms)?,
            expires_at_ms: canonical_i64(&raw.expires_at_ms)?,
            module_digests: raw
                .module_digests
                .iter()
                .map(|value| decode_hex(value).map(Digest::from_bytes))
                .collect::<Result<Vec<_>>>()?,
            peer_versions: raw.peer_versions,
            capacity: NodeCapacity {
                free_memory_bytes: canonical_u64(&raw.free_memory_bytes)?,
                free_disk_bytes: canonical_u64(&raw.free_disk_bytes)?,
                job_credits: raw.job_credits,
            },
            signature: decode_hex(&value.signature)?,
        })
    }
}

fn canonical_u64(value: &str) -> Result<u64> {
    let parsed = value
        .parse::<u64>()
        .map_err(|_| Error::Node("invalid unsigned decimal"))?;
    if parsed.to_string() != value {
        return Err(Error::Node("unsigned decimal is not canonical"));
    }
    Ok(parsed)
}

fn canonical_i64(value: &str) -> Result<i64> {
    let parsed = value
        .parse::<i64>()
        .map_err(|_| Error::Node("invalid signed decimal"))?;
    if parsed.to_string() != value {
        return Err(Error::Node("signed decimal is not canonical"));
    }
    Ok(parsed)
}

fn encode_hex(bytes: &[u8]) -> String {
    const TABLE: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(TABLE[(byte >> 4) as usize] as char);
        encoded.push(TABLE[(byte & 0x0f) as usize] as char);
    }
    encoded
}

fn decode_hex<const N: usize>(value: &str) -> Result<[u8; N]> {
    if value.len() != N * 2 {
        return Err(Error::Node("hex field length is invalid"));
    }
    let mut decoded = [0; N];
    for (index, pair) in value.as_bytes().as_chunks::<2>().0.iter().enumerate() {
        let high = nibble(pair[0]).ok_or(Error::Node("hex field is not lowercase"))?;
        let low = nibble(pair[1]).ok_or(Error::Node("hex field is not lowercase"))?;
        decoded[index] = (high << 4) | low;
    }
    Ok(decoded)
}

fn nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests;
