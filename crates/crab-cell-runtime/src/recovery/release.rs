use bytes::Bytes;
use crab_ltx::CellStorageLayout;
use crab_storage::{ETag, StorageError};
use serde::{Deserialize, Serialize};

use crate::cell::application::ApplicationIdentity;
use crate::cell::catalog::{CatalogEntry, CatalogProof, CellCatalog};
use crate::identity::Digest;
use crate::identity::RequestId;
use crate::identity::encode_hex;
use crate::registry::Registry;
use crate::{Error, Result};

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

    pub(crate) fn decode(bytes: &[u8], identity: ApplicationIdentity) -> Result<Self> {
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
            ReleaseState::Prepared | ReleaseState::Maintenance | ReleaseState::Activating
        ) && self.desired.is_none()
        {
            return Err(Error::Release(
                "release phase requires a desired descriptor",
            ));
        }
        if self.state == ReleaseState::Ready
            && (self.current.is_none() || self.current != self.desired)
        {
            return Err(Error::Release(
                "ready release requires one current desired descriptor",
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

    pub(crate) async fn install_restored(&self, record: ReleaseRecord) -> Result<ReleaseRecord> {
        record.validate(self.identity)?;
        let path = self.layout.release_path();
        match self
            .layout
            .store()
            .create_strict_with_etag(&path, Bytes::from(record.encode()?))
            .await
        {
            Ok(_) => Ok(record),
            Err(create_error) => match self.load().await? {
                Some(current) if current.record == record => Ok(current.record),
                Some(_) => Err(Error::Release(
                    "restored release conflicts with existing selection",
                )),
                None => Err(create_error.into()),
            },
        }
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
        if observed.as_ref().is_some_and(|value| {
            matches!(
                value.record.state,
                ReleaseState::Activating | ReleaseState::Maintenance
            )
        }) {
            return Err(Error::Release("release activation is already in progress"));
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

    /// Loads and verifies one immutable descriptor selected by release state.
    pub async fn descriptor(&self, digest: Digest) -> Result<Vec<u8>> {
        let path = self.layout.release_descriptor_path(digest.as_bytes());
        let (descriptor, _) = self
            .layout
            .store()
            .get_with_etag_bounded(&path, MAX_DESCRIPTOR_BYTES)
            .await?;
        if descriptor.is_empty() || blake3::hash(&descriptor).as_bytes() != digest.as_bytes() {
            return Err(Error::Release(
                "stored descriptor digest or size is invalid",
            ));
        }
        Ok(descriptor.to_vec())
    }

    /// Publishes one catalog entry under an exact ready or activating release.
    ///
    /// A failed post-publication recheck leaves the immutable catalog entry
    /// visible, so a later activation must admit it before becoming ready.
    pub async fn provision(
        &self,
        catalog: &CellCatalog,
        registry: &Registry,
        entry: CatalogEntry,
    ) -> Result<CatalogProof> {
        if !catalog.matches_identity(self.identity) {
            return Err(Error::Release(
                "catalog and release application identities differ",
            ));
        }
        let before = self
            .load()
            .await?
            .ok_or(Error::Release("release is not ready for provisioning"))?
            .record;
        let selected = selected_provision_release(&before)?;
        if selected != registry.release_digest()
            || self.descriptor(selected).await? != registry.release_bytes()
            || !registry.is_current_cell(
                entry.namespace(),
                entry.role(),
                entry.initial_code(),
                entry.initial_schema(),
            )
        {
            return Err(Error::Release(
                "catalog entry does not use the current release code and schema",
            ));
        }

        let proof = catalog.provision(entry).await?;
        let after = self
            .load()
            .await?
            .ok_or(Error::Release("release disappeared during provisioning"))?
            .record;
        if !provision_release_continues(&before, &after, selected) {
            return Err(Error::Release("release changed during provisioning"));
        }
        Ok(proof)
    }

    /// CASes one prepared release into its resumable activation phase.
    ///
    /// The caller must complete fleet and Cell compatibility admission before
    /// calling `complete_activation`; this storage owner only serializes phases.
    pub async fn start_activation(
        &self,
        expected_revision: u64,
        operation: RequestId,
    ) -> Result<ReleaseRecord> {
        let observed = self
            .load()
            .await?
            .ok_or(Error::Release("release is not prepared"))?;
        if activation_retry(&observed.record, expected_revision, operation) {
            let desired = observed
                .record
                .desired
                .ok_or(Error::Release("release activation has no descriptor"))?;
            self.descriptor(desired).await?;
            return Ok(observed.record);
        }
        if observed.record.revision != expected_revision
            || observed.record.state != ReleaseState::Prepared
            || observed.record.operation != operation
        {
            return Err(Error::Release("prepared release changed concurrently"));
        }
        let desired = observed
            .record
            .desired
            .ok_or(Error::Release("prepared release has no descriptor"))?;
        self.descriptor(desired).await?;
        let mut next = observed.record.clone();
        next.revision = next
            .revision
            .checked_add(1)
            .ok_or(Error::Release("release revision overflow"))?;
        next.state = ReleaseState::Activating;
        self.update_exact(observed, next).await
    }

    /// Enters the operation-bound offline maintenance phase.
    ///
    /// This transition only closes release admission. The caller must drain every
    /// writer and prove the advertised fleet empty before changing Cell data.
    pub async fn start_maintenance(
        &self,
        expected_revision: u64,
        operation: RequestId,
    ) -> Result<ReleaseRecord> {
        let observed = self
            .load()
            .await?
            .ok_or(Error::Release("release is not prepared"))?;
        if maintenance_retry(&observed.record, expected_revision, operation) {
            let desired = observed
                .record
                .desired
                .ok_or(Error::Release("maintenance release has no descriptor"))?;
            self.descriptor(desired).await?;
            return Ok(observed.record);
        }
        if observed.record.revision != expected_revision
            || observed.record.state != ReleaseState::Prepared
            || observed.record.operation != operation
        {
            return Err(Error::Release("prepared release changed concurrently"));
        }
        let desired = observed
            .record
            .desired
            .ok_or(Error::Release("prepared release has no descriptor"))?;
        self.descriptor(desired).await?;
        let mut next = observed.record.clone();
        next.revision = next
            .revision
            .checked_add(1)
            .ok_or(Error::Release("release revision overflow"))?;
        next.state = ReleaseState::Maintenance;
        self.update_exact(observed, next).await
    }

    /// Publishes the desired descriptor as current after caller-side admission.
    pub async fn complete_activation(
        &self,
        expected_revision: u64,
        operation: RequestId,
    ) -> Result<ReleaseRecord> {
        self.complete(
            expected_revision,
            operation,
            ReleaseState::Activating,
            "release is not activating",
            "activating release changed concurrently",
        )
        .await
    }

    /// Publishes the desired maintenance release after offline admission succeeds.
    pub async fn complete_maintenance(
        &self,
        expected_revision: u64,
        operation: RequestId,
    ) -> Result<ReleaseRecord> {
        self.complete(
            expected_revision,
            operation,
            ReleaseState::Maintenance,
            "release is not in maintenance",
            "maintenance release changed concurrently",
        )
        .await
    }

    async fn complete(
        &self,
        expected_revision: u64,
        operation: RequestId,
        required_state: ReleaseState,
        unavailable: &'static str,
        changed: &'static str,
    ) -> Result<ReleaseRecord> {
        let observed = self.load().await?.ok_or(Error::Release(unavailable))?;
        if completion_retry(&observed.record, expected_revision, operation) {
            let desired = observed
                .record
                .desired
                .ok_or(Error::Release("ready release has no descriptor"))?;
            self.descriptor(desired).await?;
            return Ok(observed.record);
        }
        if observed.record.revision != expected_revision
            || observed.record.state != required_state
            || observed.record.operation != operation
        {
            return Err(Error::Release(changed));
        }
        let desired = observed
            .record
            .desired
            .ok_or(Error::Release("activating release has no descriptor"))?;
        self.descriptor(desired).await?;
        let mut next = observed.record.clone();
        next.revision = next
            .revision
            .checked_add(1)
            .ok_or(Error::Release("release revision overflow"))?;
        next.current = Some(desired);
        next.state = ReleaseState::Ready;
        self.update_exact(observed, next).await
    }

    async fn update_exact(
        &self,
        observed: VersionedRelease,
        next: ReleaseRecord,
    ) -> Result<ReleaseRecord> {
        next.validate(self.identity)?;
        let write = self
            .layout
            .store()
            .update(
                &self.layout.release_path(),
                Bytes::from(next.encode()?),
                observed.token,
            )
            .await;
        match write {
            Ok(_) => Ok(next),
            Err(write_error) => match self.load().await? {
                Some(current) if current.record == next => Ok(current.record),
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

fn activation_retry(record: &ReleaseRecord, expected_revision: u64, operation: RequestId) -> bool {
    if record.operation != operation || record.desired.is_none() {
        return false;
    }
    match record.state {
        ReleaseState::Activating => expected_revision
            .checked_add(1)
            .is_some_and(|revision| record.revision == revision),
        ReleaseState::Ready => {
            expected_revision
                .checked_add(2)
                .is_some_and(|revision| record.revision == revision)
                && record.current == record.desired
        }
        _ => false,
    }
}

fn maintenance_retry(record: &ReleaseRecord, expected_revision: u64, operation: RequestId) -> bool {
    record.operation == operation
        && record.state == ReleaseState::Maintenance
        && record.desired.is_some()
        && expected_revision
            .checked_add(1)
            .is_some_and(|revision| record.revision == revision)
}

fn completion_retry(record: &ReleaseRecord, expected_revision: u64, operation: RequestId) -> bool {
    record.operation == operation
        && record.state == ReleaseState::Ready
        && record.current == record.desired
        && expected_revision
            .checked_add(1)
            .is_some_and(|revision| record.revision == revision)
}

fn selected_provision_release(record: &ReleaseRecord) -> Result<Digest> {
    match record.state {
        ReleaseState::Ready => record
            .current
            .filter(|current| Some(*current) == record.desired)
            .ok_or(Error::Release("ready release has no current descriptor")),
        ReleaseState::Activating => record.desired.ok_or(Error::Release(
            "activating release has no desired descriptor",
        )),
        _ => Err(Error::Release("release is not ready for provisioning")),
    }
}

fn provision_release_continues(
    before: &ReleaseRecord,
    after: &ReleaseRecord,
    selected: Digest,
) -> bool {
    if before == after {
        return true;
    }
    before.state == ReleaseState::Activating
        && after.state == ReleaseState::Ready
        && before
            .revision
            .checked_add(1)
            .is_some_and(|revision| after.revision == revision)
        && after.operation == before.operation
        && after.current == Some(selected)
        && after.desired == Some(selected)
        && after.desired_image == before.desired_image
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
    use crate::identity::{ApplicationId, TenantId};

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

    #[tokio::test]
    async fn activation_is_operation_bound_resumable_and_publishes_current() {
        let (releases, descriptor, digest) = fixture();
        let operation = RequestId::from_bytes([3; 16]);
        let prepared = releases
            .prepare(
                &descriptor,
                digest,
                0,
                &format!("sha256:{}", "a".repeat(64)),
                operation,
            )
            .await
            .unwrap();

        let activating = releases
            .start_activation(prepared.revision(), operation)
            .await
            .unwrap();
        assert_eq!(activating.revision(), 2);
        assert_eq!(activating.state(), ReleaseState::Activating);
        assert_eq!(activating.current(), None);
        assert_eq!(
            releases
                .start_activation(prepared.revision(), operation)
                .await
                .unwrap(),
            activating
        );

        let ready = releases
            .complete_activation(activating.revision(), operation)
            .await
            .unwrap();
        assert_eq!(ready.revision(), 3);
        assert_eq!(ready.state(), ReleaseState::Ready);
        assert_eq!(ready.current(), Some(digest));
        assert_eq!(ready.current(), ready.desired());
        assert_eq!(
            releases
                .start_activation(prepared.revision(), operation)
                .await
                .unwrap(),
            ready
        );
        assert_eq!(
            releases
                .complete_activation(activating.revision(), operation)
                .await
                .unwrap(),
            ready
        );
    }

    #[tokio::test]
    async fn activation_rejects_operation_revision_and_descriptor_drift() {
        let (releases, descriptor, digest) = fixture();
        let operation = RequestId::from_bytes([3; 16]);
        let prepared = releases
            .prepare(
                &descriptor,
                digest,
                0,
                &format!("sha256:{}", "a".repeat(64)),
                operation,
            )
            .await
            .unwrap();
        assert!(
            releases
                .start_activation(prepared.revision(), RequestId::from_bytes([4; 16]))
                .await
                .is_err()
        );
        assert!(
            releases
                .start_activation(prepared.revision() + 1, operation)
                .await
                .is_err()
        );

        let path = releases.layout.release_descriptor_path(digest.as_bytes());
        releases
            .layout
            .store()
            .put_overwrite(&path, Bytes::from_static(b"corrupt"))
            .await
            .unwrap();
        assert!(
            releases
                .start_activation(prepared.revision(), operation)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn maintenance_is_operation_bound_and_resumable() {
        let (releases, descriptor, digest) = fixture();
        let operation = RequestId::from_bytes([3; 16]);
        let prepared = releases
            .prepare(
                &descriptor,
                digest,
                0,
                &format!("sha256:{}", "a".repeat(64)),
                operation,
            )
            .await
            .unwrap();

        let maintenance = releases
            .start_maintenance(prepared.revision(), operation)
            .await
            .unwrap();
        assert_eq!(maintenance.revision(), 2);
        assert_eq!(maintenance.state(), ReleaseState::Maintenance);
        assert_eq!(maintenance.current(), None);
        assert_eq!(maintenance.desired(), Some(digest));
        assert_eq!(
            releases
                .start_maintenance(prepared.revision(), operation)
                .await
                .unwrap(),
            maintenance
        );
        assert!(
            releases
                .start_activation(prepared.revision(), operation)
                .await
                .is_err()
        );
        assert!(
            releases
                .prepare(
                    &descriptor,
                    digest,
                    maintenance.revision(),
                    &format!("sha256:{}", "b".repeat(64)),
                    RequestId::from_bytes([4; 16]),
                )
                .await
                .is_err()
        );
        let ready = releases
            .complete_maintenance(maintenance.revision(), operation)
            .await
            .unwrap();
        assert_eq!(ready.revision(), 3);
        assert_eq!(ready.state(), ReleaseState::Ready);
        assert_eq!(ready.current(), Some(digest));
        assert_eq!(
            releases
                .complete_maintenance(maintenance.revision(), operation)
                .await
                .unwrap(),
            ready
        );
    }

    #[tokio::test]
    async fn prepare_cannot_replace_an_activation_in_progress() {
        let (releases, descriptor, digest) = fixture();
        let operation = RequestId::from_bytes([3; 16]);
        let prepared = releases
            .prepare(
                &descriptor,
                digest,
                0,
                &format!("sha256:{}", "a".repeat(64)),
                operation,
            )
            .await
            .unwrap();
        let activating = releases
            .start_activation(prepared.revision(), operation)
            .await
            .unwrap();

        assert!(
            releases
                .prepare(
                    &descriptor,
                    digest,
                    activating.revision(),
                    &format!("sha256:{}", "b".repeat(64)),
                    RequestId::from_bytes([5; 16]),
                )
                .await
                .is_err()
        );
    }

    #[test]
    fn provisioning_recheck_accepts_only_the_exact_activation_completion() {
        let (_, _, digest) = fixture();
        let identity = ApplicationIdentity::new(
            TenantId::from_bytes([1; 16]),
            ApplicationId::from_bytes([2; 16]),
        );
        let before = ReleaseRecord {
            application: identity.application(),
            revision: 2,
            current: None,
            desired: Some(digest),
            desired_image: format!("sha256:{}", "a".repeat(64)),
            operation: RequestId::from_bytes([3; 16]),
            state: ReleaseState::Activating,
        };
        let mut completed = before.clone();
        completed.revision = 3;
        completed.current = Some(digest);
        completed.state = ReleaseState::Ready;
        assert!(provision_release_continues(&before, &completed, digest));

        completed.operation = RequestId::from_bytes([4; 16]);
        assert!(!provision_release_continues(&before, &completed, digest));
    }
}
