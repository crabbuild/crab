use bytes::Bytes;
use crab_storage::{CellStorageLayout, ETag, StorageError};
use serde::{Deserialize, Serialize};

use crate::{ApplicationIdentity, Digest, Error, RequestId, Result, identity::encode_hex};

const MAX_RELEASE_BYTES: u64 = 8 * 1024;
const MAX_DESCRIPTOR_BYTES: u64 = 256 * 1024;

/// Durable rollout phase for one compiled application release.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReleaseState {
    Ready,
    Prepared,
    Maintenance,
    Activating,
    Failed,
}

/// Canonical mutable release selection for one application.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReleaseRecord {
    application: crate::ApplicationId,
    revision: u64,
    current: Option<Digest>,
    desired: Option<Digest>,
    desired_image: String,
    operation: RequestId,
    state: ReleaseState,
}

impl ReleaseRecord {
    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.revision
    }

    #[must_use]
    pub const fn current(&self) -> Option<Digest> {
        self.current
    }

    #[must_use]
    pub const fn desired(&self) -> Option<Digest> {
        self.desired
    }

    #[must_use]
    pub fn desired_image(&self) -> &str {
        &self.desired_image
    }

    #[must_use]
    pub const fn operation(&self) -> RequestId {
        self.operation
    }

    #[must_use]
    pub const fn state(&self) -> ReleaseState {
        self.state
    }

    /// Encodes the exact canonical JSON body stored and printed by administration.
    pub fn encode(&self) -> Result<Vec<u8>> {
        let bytes = serde_json::to_vec(&RawRelease {
            application: encode_hex(self.application.as_bytes()),
            current: self.current.map(|value| encode_hex(value.as_bytes())),
            desired: self.desired.map(|value| encode_hex(value.as_bytes())),
            desired_image: self.desired_image.clone(),
            operation: encode_hex(self.operation.as_bytes()),
            revision: self.revision.to_string(),
            state: self.state,
            version: 1,
        })?;
        if bytes.len() as u64 > MAX_RELEASE_BYTES {
            return Err(Error::Release("release record exceeds 8 KiB"));
        }
        Ok(bytes)
    }

    fn decode(bytes: &[u8], identity: ApplicationIdentity) -> Result<Self> {
        let raw: RawRelease = serde_json::from_slice(bytes)?;
        if raw.version != 1 {
            return Err(Error::Release("unsupported release version"));
        }
        let record = Self {
            application: crate::ApplicationId::from_bytes(parse_hex(&raw.application)?),
            revision: parse_revision(&raw.revision)?,
            current: raw.current.as_deref().map(parse_digest).transpose()?,
            desired: raw.desired.as_deref().map(parse_digest).transpose()?,
            desired_image: raw.desired_image,
            operation: RequestId::from_bytes(parse_hex(&raw.operation)?),
            state: raw.state,
        };
        record.validate(identity)?;
        if record.encode()?.as_slice() != bytes {
            return Err(Error::Release("release record is not canonical"));
        }
        Ok(record)
    }

    fn validate(&self, identity: ApplicationIdentity) -> Result<()> {
        if self.application != identity.application() || self.revision == 0 {
            return Err(Error::Release("release identity or revision is invalid"));
        }
        if self.operation.as_bytes().iter().all(|byte| *byte == 0)
            || self
                .current
                .is_some_and(|digest| digest.as_bytes().iter().all(|byte| *byte == 0))
            || self
                .desired
                .is_some_and(|digest| digest.as_bytes().iter().all(|byte| *byte == 0))
        {
            return Err(Error::Release("release IDs and digests must be nonzero"));
        }
        validate_image(&self.desired_image)?;
        if matches!(
            self.state,
            ReleaseState::Prepared | ReleaseState::Activating
        ) && self.desired.is_none()
        {
            return Err(Error::Release(
                "release phase requires a desired descriptor",
            ));
        }
        Ok(())
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RawRelease {
    application: String,
    current: Option<String>,
    desired: Option<String>,
    desired_image: String,
    operation: String,
    revision: String,
    state: ReleaseState,
    version: u8,
}

/// One exact release observation and its conditional-write token.
pub struct VersionedRelease {
    record: ReleaseRecord,
    token: ETag,
}

impl VersionedRelease {
    #[must_use]
    pub const fn record(&self) -> &ReleaseRecord {
        &self.record
    }
}

/// Publishes immutable descriptors before selecting them through release CAS.
#[derive(Clone)]
pub struct ReleaseStore {
    layout: CellStorageLayout,
    identity: ApplicationIdentity,
}

impl ReleaseStore {
    pub fn new(layout: CellStorageLayout, identity: ApplicationIdentity) -> Result<Self> {
        if layout.application_id() != identity.application().as_bytes() {
            return Err(Error::Release("layout and application identity differ"));
        }
        Ok(Self { layout, identity })
    }

    /// Loads and verifies the current release selection.
    pub async fn load(&self) -> Result<Option<VersionedRelease>> {
        let (bytes, token) = match self
            .layout
            .store()
            .get_with_etag_bounded(&self.layout.release_path(), MAX_RELEASE_BYTES)
            .await
        {
            Ok(value) => value,
            Err(StorageError::NotFound { .. }) => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        Ok(Some(VersionedRelease {
            record: ReleaseRecord::decode(&bytes, self.identity)?,
            token,
        }))
    }

    /// Uploads one compiled descriptor and conditionally selects it as prepared.
    pub async fn prepare(
        &self,
        descriptor: &[u8],
        digest: Digest,
        expected_revision: u64,
        image: &str,
        operation: RequestId,
    ) -> Result<ReleaseRecord> {
        if descriptor.is_empty()
            || descriptor.len() as u64 > MAX_DESCRIPTOR_BYTES
            || blake3::hash(descriptor).as_bytes() != digest.as_bytes()
        {
            return Err(Error::Release(
                "compiled descriptor digest or size is invalid",
            ));
        }
        validate_image(image)?;
        self.publish_descriptor(descriptor, digest).await?;

        let observed = self.load().await?;
        if let Some(observed) = &observed
            && prepared_matches(
                &observed.record,
                digest,
                expected_revision,
                image,
                operation,
            )
        {
            return Ok(observed.record.clone());
        }
        if observed.as_ref().map_or(0, |value| value.record.revision) != expected_revision {
            return Err(Error::Release("release revision changed concurrently"));
        }
        let next = ReleaseRecord {
            application: self.identity.application(),
            revision: expected_revision
                .checked_add(1)
                .ok_or(Error::Release("release revision overflow"))?,
            current: observed.as_ref().and_then(|value| value.record.current),
            desired: Some(digest),
            desired_image: image.to_owned(),
            operation,
            state: ReleaseState::Prepared,
        };
        next.validate(self.identity)?;
        let write = match observed {
            Some(observed) => {
                self.layout
                    .store()
                    .update(
                        &self.layout.release_path(),
                        Bytes::from(next.encode()?),
                        observed.token,
                    )
                    .await
            }
            None => {
                self.layout
                    .store()
                    .create_strict_with_etag(
                        &self.layout.release_path(),
                        Bytes::from(next.encode()?),
                    )
                    .await
            }
        };
        match write {
            Ok(_) => Ok(next),
            Err(write_error) => match self.load().await? {
                Some(current)
                    if current.record.revision == next.revision
                        && current.record.current == next.current
                        && current.record.desired == next.desired
                        && current.record.desired_image == next.desired_image
                        && current.record.operation == next.operation
                        && current.record.state == next.state =>
                {
                    Ok(current.record)
                }
                Some(_) => Err(Error::Release("release changed concurrently")),
                None => Err(write_error.into()),
            },
        }
    }

    async fn publish_descriptor(&self, descriptor: &[u8], digest: Digest) -> Result<()> {
        let path = self.layout.release_descriptor_path(digest.as_bytes());
        match self
            .layout
            .store()
            .create_strict(&path, Bytes::copy_from_slice(descriptor))
            .await
        {
            Ok(()) => Ok(()),
            Err(create_error) => {
                let (current, _) = self
                    .layout
                    .store()
                    .get_with_etag_bounded(&path, MAX_DESCRIPTOR_BYTES)
                    .await?;
                if current.as_ref() == descriptor {
                    Ok(())
                } else {
                    let _ = create_error;
                    Err(Error::Release(
                        "descriptor digest path contains different bytes",
                    ))
                }
            }
        }
    }
}

fn prepared_matches(
    record: &ReleaseRecord,
    digest: Digest,
    expected_revision: u64,
    image: &str,
    operation: RequestId,
) -> bool {
    let Some(next_revision) = expected_revision.checked_add(1) else {
        return false;
    };
    record.revision == next_revision
        && record.desired == Some(digest)
        && record.desired_image == image
        && record.state == ReleaseState::Prepared
        && record.operation == operation
}

fn validate_image(image: &str) -> Result<()> {
    let Some(digest) = image.strip_prefix("sha256:") else {
        return Err(Error::Release("image must be a sha256 digest"));
    };
    if digest.len() != 64
        || !digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(Error::Release("image must be a lowercase sha256 digest"));
    }
    Ok(())
}

fn parse_hex<const N: usize>(value: &str) -> Result<[u8; N]> {
    crate::identity::decode_hex(value).map_err(|_| Error::Release("invalid lowercase hex field"))
}

fn parse_digest(value: &str) -> Result<Digest> {
    Ok(Digest::from_bytes(parse_hex(value)?))
}

fn parse_revision(value: &str) -> Result<u64> {
    if value.is_empty()
        || (value.len() > 1 && value.starts_with('0'))
        || !value.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(Error::Release("invalid release revision"));
    }
    value
        .parse()
        .map_err(|_| Error::Release("invalid release revision"))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crab_storage::Store;
    use object_store::{memory::InMemory, path::Path};

    use super::*;
    use crate::{ApplicationId, TenantId};

    fn fixture() -> (ReleaseStore, Vec<u8>, Digest) {
        let identity = ApplicationIdentity::new(
            TenantId::from_bytes([1; 16]),
            ApplicationId::from_bytes([2; 16]),
        );
        let layout = CellStorageLayout::new(
            Store::new(Arc::new(InMemory::new())),
            Path::from("root"),
            *identity.application().as_bytes(),
        );
        let descriptor = br#"{"runtime":"crab-http-server","version":1}"#.to_vec();
        let digest = Digest::from_bytes(*blake3::hash(&descriptor).as_bytes());
        (
            ReleaseStore::new(layout, identity).unwrap(),
            descriptor,
            digest,
        )
    }

    #[tokio::test]
    async fn prepare_uploads_descriptor_before_cas_and_reconciles_retry() {
        let (releases, descriptor, digest) = fixture();
        let image = format!("sha256:{}", "a".repeat(64));
        let first = releases
            .prepare(
                &descriptor,
                digest,
                0,
                &image,
                RequestId::from_bytes([3; 16]),
            )
            .await
            .unwrap();
        assert_eq!(first.revision(), 1);
        assert_eq!(first.desired(), Some(digest));
        assert_eq!(first.state(), ReleaseState::Prepared);

        let retry = releases
            .prepare(
                &descriptor,
                digest,
                0,
                &image,
                RequestId::from_bytes([3; 16]),
            )
            .await
            .unwrap();
        assert_eq!(retry, first);
        assert!(
            releases
                .prepare(
                    &descriptor,
                    digest,
                    1,
                    &format!("sha256:{}", "b".repeat(64)),
                    RequestId::from_bytes([5; 16]),
                )
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn prepare_rejects_digest_image_and_revision_drift() {
        let (releases, descriptor, digest) = fixture();
        let image = format!("sha256:{}", "a".repeat(64));
        assert!(matches!(
            releases
                .prepare(
                    &descriptor,
                    Digest::from_bytes([9; 32]),
                    0,
                    &image,
                    RequestId::from_bytes([3; 16]),
                )
                .await,
            Err(Error::Release(_))
        ));
        assert!(matches!(
            releases
                .prepare(
                    &descriptor,
                    digest,
                    0,
                    "latest",
                    RequestId::from_bytes([3; 16]),
                )
                .await,
            Err(Error::Release(_))
        ));
        releases
            .prepare(
                &descriptor,
                digest,
                0,
                &image,
                RequestId::from_bytes([3; 16]),
            )
            .await
            .unwrap();
        assert!(matches!(
            releases
                .prepare(
                    &descriptor,
                    digest,
                    9,
                    &image,
                    RequestId::from_bytes([3; 16]),
                )
                .await,
            Err(Error::Release(_))
        ));
    }
}
