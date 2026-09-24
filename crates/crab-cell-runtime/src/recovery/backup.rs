//! Backup pins over revision-pinned catalog shards and pages.
use std::collections::BTreeMap;

use bytes::Bytes;
use crab_ltx::CellStorageLayout;
use crab_storage::StorageError;
use serde::{Deserialize, Serialize};

use crate::cell::application::ApplicationIdentity;
use crate::cell::catalog::CellCatalog;
use crate::control::Control;
use crate::identity::Digest;
use crate::identity::RequestId;
use crate::identity::{decode_hex, encode_hex};
use crate::ltx::{Host as ReplicaHost, Limits as ReplicaLimits};
use crate::recovery::release::{ReleaseRecord, ReleaseState, ReleaseStore};
use crate::{Error, Result};

mod restore;
pub use restore::BackupRestore;

const MAX_PIN_BYTES: u64 = 32 * 1024;
const MAX_SHARD_BYTES: u64 = 256 * 1024;
const MAX_PAGE_BYTES: u64 = 1024 * 1024;
const MAX_RELEASE_SNAPSHOT_BYTES: u64 = 24 * 1024;
const MAX_PAGES_PER_SHARD: usize = 2048;

/// Immutable backup boundary published after every referenced Cell root verifies.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackupPin {
    application: crate::ApplicationId,
    id: RequestId,
    created_at_ms: i64,
    control_count: u64,
    catalog_revisions: Vec<u64>,
    release: [u8; 32],
    shards: Vec<ShardRef>,
}

impl BackupPin {
    /// Returns the request that created the pin.
    #[must_use]
    pub const fn id(&self) -> RequestId {
        self.id
    }

    /// Returns the logical time the pin was created.
    #[must_use]
    pub const fn created_at_ms(&self) -> i64 {
        self.created_at_ms
    }

    /// Returns how many control records the pin covers.
    #[must_use]
    pub const fn control_count(&self) -> u64 {
        self.control_count
    }

    /// Returns the application the pin belongs to.
    #[must_use]
    pub const fn application(&self) -> crate::ApplicationId {
        self.application
    }

    /// Returns the catalog revision the pin covers for each shard.
    #[must_use]
    pub fn catalog_revisions(&self) -> &[u64] {
        &self.catalog_revisions
    }

    /// Returns the release digest the pin covers.
    #[must_use]
    pub const fn release_digest(&self) -> [u8; 32] {
        self.release
    }
}

/// One revision-pinned catalog shard and its immutable page dependencies.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PinnedCatalogShard {
    /// Catalog shard the record pins.
    pub shard: u8,
    /// Revision the shard was pinned at.
    pub revision: u64,
    /// Immutable page digests the shard referenced.
    pub pages: Vec<crate::Digest>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ShardRef {
    shard: u8,
    digest: [u8; 32],
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PinWire {
    application: String,
    catalog_revisions: Vec<String>,
    control_count: String,
    created_at_ms: String,
    pin: String,
    release: String,
    shards: Vec<ShardRefWire>,
    version: u8,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ShardRefWire {
    digest: String,
    shard: u8,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ShardWire {
    catalog_pages: Vec<String>,
    catalog_revision: String,
    control_count: String,
    control_pages: Vec<String>,
    shard: u8,
    version: u8,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PageWire {
    controls: Vec<String>,
    version: u8,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ReleaseWire {
    descriptors: Vec<String>,
    record: String,
    version: u8,
}

/// Creates and reopens application-scoped backup pins.
///
/// Immutable pages and shard manifests may be left behind by interruption. The
/// pin pointer is strict-created only after every exact LTX graph verifies.
#[derive(Clone)]
pub struct BackupPinStore {
    layout: CellStorageLayout,
    identity: ApplicationIdentity,
    limits: ReplicaLimits,
    host: ReplicaHost,
}

impl BackupPinStore {
    /// Creates a backup-pin store after checking that the layout belongs to the
    /// same application as the identity.
    pub fn new(
        layout: CellStorageLayout,
        identity: ApplicationIdentity,
        limits: ReplicaLimits,
        host: ReplicaHost,
    ) -> Result<Self> {
        if layout.application_id() != identity.application().as_bytes() {
            return Err(Error::Backup("layout and application identity differ"));
        }
        Ok(Self {
            layout,
            identity,
            limits,
            host,
        })
    }

    /// Verifies exact roots, uploads immutable pages, and strict-creates one pin.
    ///
    /// The caller must remain enrolled in the application's maintenance drain
    /// from before its final Ready check until this future returns. This method
    /// validates Ready state but cannot atomically fence a separate collector.
    pub async fn create(
        &self,
        id: RequestId,
        created_at_ms: i64,
        catalog: Vec<PinnedCatalogShard>,
        mut controls: Vec<Control>,
    ) -> Result<BackupPin> {
        if id.as_bytes().iter().all(|byte| *byte == 0) || created_at_ms < 0 {
            return Err(Error::Backup("pin identity or timestamp is invalid"));
        }
        if catalog.len() != 256
            || catalog
                .iter()
                .enumerate()
                .any(|(index, shard)| usize::from(shard.shard) != index)
        {
            return Err(Error::Backup("pin must name all 256 catalog shards"));
        }
        let catalog_store = CellCatalog::new(self.layout.clone(), self.identity.tenant());
        let mut catalog_cells = Vec::new();
        for shard in &catalog {
            catalog_cells.extend(
                catalog_store
                    .pinned_cells(shard.shard, shard.revision, &shard.pages)
                    .await?,
            );
        }
        let catalog_revisions = catalog.iter().map(|shard| shard.revision).collect();
        controls.sort_unstable_by_key(|control| *control.cell.as_bytes());
        if controls.windows(2).any(|pair| pair[0].cell == pair[1].cell) {
            return Err(Error::Backup("pin contains duplicate Cell controls"));
        }
        if controls
            .iter()
            .map(|control| control.cell)
            .ne(catalog_cells.iter().copied())
        {
            return Err(Error::Backup("pin controls differ from the pinned catalog"));
        }
        for control in &controls {
            control.encode()?;
            self.verify_root(control).await?;
        }
        let release = self.snapshot_release().await?;

        let mut groups = BTreeMap::<u8, Vec<Control>>::new();
        for control in controls {
            groups
                .entry(control.cell.as_bytes()[0])
                .or_default()
                .push(control);
        }
        let mut shards = Vec::with_capacity(groups.len());
        let mut control_count = 0u64;
        for catalog_shard in catalog {
            let shard = catalog_shard.shard;
            let controls = groups.remove(&shard).unwrap_or_default();
            let shard_control_count = controls.len() as u64;
            control_count = control_count
                .checked_add(shard_control_count)
                .ok_or(Error::Backup("pin control count overflow"))?;
            let mut pages = Vec::new();
            let mut page = Vec::new();
            for control in controls {
                let encoded = hex(&control.encode()?);
                page.push(encoded);
                if page_bytes(&page)?.len() as u64 > MAX_PAGE_BYTES {
                    let last = page
                        .pop()
                        .ok_or(Error::Backup("pin page construction failed"))?;
                    if page.is_empty() {
                        return Err(Error::Backup("one control exceeds the pin page limit"));
                    }
                    pages.push(self.publish_page(&page).await?);
                    page.push(last);
                }
            }
            if !page.is_empty() {
                pages.push(self.publish_page(&page).await?);
            }
            if pages.len() > MAX_PAGES_PER_SHARD {
                return Err(Error::Backup("pin shard page count is invalid"));
            }
            if pages.is_empty() && catalog_shard.pages.is_empty() {
                continue;
            }
            let catalog_pages = catalog_shard
                .pages
                .iter()
                .map(|digest| *digest.as_bytes())
                .collect::<Vec<_>>();
            let body = encode_shard(
                shard,
                catalog_shard.revision,
                &catalog_pages,
                shard_control_count,
                &pages,
            )?;
            let digest = publish_object(&self.layout, body).await?;
            shards.push(ShardRef { shard, digest });
        }
        let pin = BackupPin {
            application: self.identity.application(),
            id,
            created_at_ms,
            control_count,
            catalog_revisions,
            release,
            shards,
        };
        let body = pin.encode()?;
        let path = self.layout.pin_path(id.as_bytes());
        match self
            .layout
            .store()
            .create_strict(&path, Bytes::from(body.clone()))
            .await
        {
            Ok(()) => Ok(pin),
            Err(create_error) => match self.load(id).await? {
                Some(existing) if existing.encode()? == body => Ok(existing),
                Some(_) => Err(Error::Backup("pin ID already names different contents")),
                None => Err(create_error.into()),
            },
        }
    }

    /// Loads one exact pin pointer without traversing its immutable pages.
    pub async fn load(&self, id: RequestId) -> Result<Option<BackupPin>> {
        let (body, _) = match self
            .layout
            .store()
            .get_with_etag_bounded(&self.layout.pin_path(id.as_bytes()), MAX_PIN_BYTES)
            .await
        {
            Ok(value) => value,
            Err(StorageError::NotFound { .. }) => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let pin = BackupPin::decode(&body)?;
        if pin.application != self.identity.application() || pin.id != id {
            return Err(Error::Backup("pin pointer scope does not match its path"));
        }
        Ok(Some(pin))
    }

    /// Loads every canonical control captured by one pin.
    pub async fn controls(&self, pin: &BackupPin) -> Result<Vec<Control>> {
        Ok(self.manifest(pin).await?.controls)
    }

    /// Reopens every immutable dependency captured by one pin.
    pub async fn verify(&self, pin: &BackupPin) -> Result<Vec<Control>> {
        let controls = self.controls(pin).await?;
        for control in &controls {
            self.verify_root(control).await?;
        }
        Ok(controls)
    }

    async fn snapshot_release(&self) -> Result<[u8; 32]> {
        let releases = ReleaseStore::new(self.layout.clone(), self.identity)?;
        let record = releases
            .load()
            .await?
            .ok_or(Error::Backup("release is absent from backup scope"))?
            .record()
            .clone();
        if record.state() != ReleaseState::Ready {
            return Err(Error::Backup("backup release is not ready"));
        }
        let descriptors = release_descriptors(&record);
        for digest in &descriptors {
            releases.descriptor(*digest).await?;
        }
        publish_object(&self.layout, encode_release(&record, &descriptors)?).await
    }

    async fn load_release(&self, digest: [u8; 32]) -> Result<(ReleaseRecord, Vec<Digest>)> {
        let body = self
            .read_object(&digest, MAX_RELEASE_SNAPSHOT_BYTES)
            .await?;
        let (record, descriptors) = decode_release(&body, self.identity)?;
        let releases = ReleaseStore::new(self.layout.clone(), self.identity)?;
        for digest in &descriptors {
            releases.descriptor(*digest).await?;
        }
        Ok((record, descriptors))
    }

    async fn verify_root(&self, control: &Control) -> Result<()> {
        if let Some(root) = control.ltx_root() {
            crab_ltx::CellReplica::new(
                self.layout.clone(),
                *control.cell.as_bytes(),
                *control.incarnation.as_bytes(),
                self.limits,
            )?
            .with_host(self.host.clone())
            .reachable_objects(&root)
            .await?;
        }
        Ok(())
    }

    async fn publish_page(&self, controls: &[String]) -> Result<[u8; 32]> {
        publish_object(&self.layout, encode_page(controls)?).await
    }

    async fn read_object(&self, digest: &[u8; 32], limit: u64) -> Result<Vec<u8>> {
        let (body, _) = self
            .layout
            .store()
            .get_with_etag_bounded(&self.layout.pin_object_path(digest), limit)
            .await?;
        if blake3::hash(&body).as_bytes() != digest {
            return Err(Error::Backup("pin object digest mismatch"));
        }
        Ok(body.to_vec())
    }
}

impl BackupPin {
    fn encode(&self) -> Result<Vec<u8>> {
        if self.created_at_ms < 0
            || self.id.as_bytes().iter().all(|byte| *byte == 0)
            || self.catalog_revisions.len() != 256
            || self.shards.len() > 256
            || self
                .shards
                .windows(2)
                .any(|pair| pair[0].shard >= pair[1].shard)
            || self
                .catalog_revisions
                .iter()
                .enumerate()
                .any(|(shard, revision)| {
                    *revision != 0
                        && !self
                            .shards
                            .iter()
                            .any(|reference| usize::from(reference.shard) == shard)
                })
        {
            return Err(Error::Backup("pin pointer bounds are invalid"));
        }
        let body = serde_json::to_vec(&PinWire {
            application: encode_hex(self.application.as_bytes()),
            catalog_revisions: self.catalog_revisions.iter().map(u64::to_string).collect(),
            control_count: self.control_count.to_string(),
            created_at_ms: self.created_at_ms.to_string(),
            pin: encode_hex(self.id.as_bytes()),
            release: encode_hex(&self.release),
            shards: self
                .shards
                .iter()
                .map(|shard| ShardRefWire {
                    digest: encode_hex(&shard.digest),
                    shard: shard.shard,
                })
                .collect(),
            version: 1,
        })?;
        if body.len() as u64 > MAX_PIN_BYTES {
            return Err(Error::Backup("pin pointer exceeds 32 KiB"));
        }
        Ok(body)
    }

    fn decode(body: &[u8]) -> Result<Self> {
        if body.len() as u64 > MAX_PIN_BYTES {
            return Err(Error::Backup("pin pointer exceeds 32 KiB"));
        }
        let wire: PinWire = serde_json::from_slice(body)?;
        if wire.version != 1 {
            return Err(Error::Backup("unsupported pin pointer version"));
        }
        let pin = Self {
            application: crate::ApplicationId::from_bytes(decode_hex(&wire.application)?),
            catalog_revisions: wire
                .catalog_revisions
                .iter()
                .map(|revision| canonical_u64(revision))
                .collect::<Result<Vec<_>>>()?,
            id: RequestId::from_bytes(decode_hex(&wire.pin)?),
            created_at_ms: canonical_i64(&wire.created_at_ms)?,
            control_count: canonical_u64(&wire.control_count)?,
            release: decode_hex(&wire.release)?,
            shards: wire
                .shards
                .into_iter()
                .map(|shard| {
                    Ok(ShardRef {
                        shard: shard.shard,
                        digest: decode_hex(&shard.digest)?,
                    })
                })
                .collect::<Result<Vec<_>>>()?,
        };
        if pin.encode()?.as_slice() != body {
            return Err(Error::Backup("pin pointer is not canonical"));
        }
        Ok(pin)
    }
}

struct ShardManifest {
    catalog_revision: u64,
    catalog_pages: Vec<[u8; 32]>,
    control_count: u64,
    control_pages: Vec<[u8; 32]>,
}

fn encode_page(controls: &[String]) -> Result<Vec<u8>> {
    if controls.is_empty() {
        return Err(Error::Backup("pin page is empty"));
    }
    let body = page_bytes(controls)?;
    if body.len() as u64 > MAX_PAGE_BYTES {
        return Err(Error::Backup("pin page exceeds 1 MiB"));
    }
    Ok(body)
}

fn page_bytes(controls: &[String]) -> Result<Vec<u8>> {
    Ok(serde_json::to_vec(&PageWire {
        controls: controls.to_vec(),
        version: 1,
    })?)
}

fn decode_page(body: &[u8]) -> Result<Vec<String>> {
    if body.len() as u64 > MAX_PAGE_BYTES {
        return Err(Error::Backup("pin page exceeds 1 MiB"));
    }
    let page: PageWire = serde_json::from_slice(body)?;
    if page.version != 1 || page.controls.is_empty() || encode_page(&page.controls)? != body {
        return Err(Error::Backup("pin page is not canonical"));
    }
    Ok(page.controls)
}

fn encode_shard(
    shard: u8,
    catalog_revision: u64,
    catalog_pages: &[[u8; 32]],
    control_count: u64,
    control_pages: &[[u8; 32]],
) -> Result<Vec<u8>> {
    if (catalog_revision == 0) != catalog_pages.is_empty()
        || catalog_pages.len() > 256
        || (control_count == 0) != control_pages.is_empty()
        || control_pages.len() > MAX_PAGES_PER_SHARD
        || (catalog_pages.is_empty() && control_pages.is_empty())
    {
        return Err(Error::Backup("pin shard bounds are invalid"));
    }
    let body = serde_json::to_vec(&ShardWire {
        catalog_pages: catalog_pages
            .iter()
            .map(|digest| encode_hex(digest))
            .collect(),
        catalog_revision: catalog_revision.to_string(),
        control_count: control_count.to_string(),
        control_pages: control_pages
            .iter()
            .map(|digest| encode_hex(digest))
            .collect(),
        shard,
        version: 1,
    })?;
    if body.len() as u64 > MAX_SHARD_BYTES {
        return Err(Error::Backup("pin shard exceeds 256 KiB"));
    }
    Ok(body)
}

fn decode_shard(body: &[u8], expected_shard: u8) -> Result<ShardManifest> {
    if body.len() as u64 > MAX_SHARD_BYTES {
        return Err(Error::Backup("pin shard exceeds 256 KiB"));
    }
    let wire: ShardWire = serde_json::from_slice(body)?;
    if wire.version != 1 || wire.shard != expected_shard {
        return Err(Error::Backup("pin shard identity differs"));
    }
    let manifest = ShardManifest {
        catalog_revision: canonical_u64(&wire.catalog_revision)?,
        catalog_pages: wire
            .catalog_pages
            .iter()
            .map(|digest| decode_hex(digest))
            .collect::<Result<Vec<_>>>()?,
        control_count: canonical_u64(&wire.control_count)?,
        control_pages: wire
            .control_pages
            .iter()
            .map(|digest| decode_hex(digest))
            .collect::<Result<Vec<_>>>()?,
    };
    if encode_shard(
        wire.shard,
        manifest.catalog_revision,
        &manifest.catalog_pages,
        manifest.control_count,
        &manifest.control_pages,
    )? != body
    {
        return Err(Error::Backup("pin shard is not canonical"));
    }
    Ok(manifest)
}

fn release_descriptors(record: &ReleaseRecord) -> Vec<Digest> {
    let mut descriptors = [record.current(), record.desired()]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    descriptors.sort_unstable_by_key(|digest| *digest.as_bytes());
    descriptors.dedup();
    descriptors
}

fn encode_release(record: &ReleaseRecord, descriptors: &[Digest]) -> Result<Vec<u8>> {
    if descriptors != release_descriptors(record) {
        return Err(Error::Backup(
            "release descriptor set differs from its record",
        ));
    }
    let body = serde_json::to_vec(&ReleaseWire {
        descriptors: descriptors
            .iter()
            .map(|digest| encode_hex(digest.as_bytes()))
            .collect(),
        record: encode_hex(&record.encode()?),
        version: 1,
    })?;
    if body.len() as u64 > MAX_RELEASE_SNAPSHOT_BYTES {
        return Err(Error::Backup("release snapshot exceeds 24 KiB"));
    }
    Ok(body)
}

fn decode_release(
    body: &[u8],
    identity: ApplicationIdentity,
) -> Result<(ReleaseRecord, Vec<Digest>)> {
    if body.len() as u64 > MAX_RELEASE_SNAPSHOT_BYTES {
        return Err(Error::Backup("release snapshot exceeds 24 KiB"));
    }
    let wire: ReleaseWire = serde_json::from_slice(body)?;
    if wire.version != 1 {
        return Err(Error::Backup("unsupported release snapshot version"));
    }
    let record = ReleaseRecord::decode(&unhex(&wire.record)?, identity)?;
    let descriptors = wire
        .descriptors
        .iter()
        .map(|digest| decode_hex(digest).map(Digest::from_bytes))
        .collect::<Result<Vec<_>>>()?;
    if encode_release(&record, &descriptors)? != body {
        return Err(Error::Backup("release snapshot is not canonical"));
    }
    Ok((record, descriptors))
}

async fn publish_object(layout: &CellStorageLayout, body: Vec<u8>) -> Result<[u8; 32]> {
    let digest = *blake3::hash(&body).as_bytes();
    layout
        .store()
        .put(&layout.pin_object_path(&digest), Bytes::from(body))
        .await?;
    Ok(digest)
}

fn canonical_u64(value: &str) -> Result<u64> {
    let parsed = value
        .parse::<u64>()
        .map_err(|_| Error::Backup("pin number is invalid"))?;
    if parsed.to_string() != value {
        return Err(Error::Backup("pin number is not canonical"));
    }
    Ok(parsed)
}

fn canonical_i64(value: &str) -> Result<i64> {
    let parsed = value
        .parse::<i64>()
        .map_err(|_| Error::Backup("pin timestamp is invalid"))?;
    if parsed < 0 || parsed.to_string() != value {
        return Err(Error::Backup("pin timestamp is not canonical"));
    }
    Ok(parsed)
}

fn hex(bytes: &[u8]) -> String {
    encode_hex(bytes)
}

fn unhex(value: &str) -> Result<Vec<u8>> {
    if !value.len().is_multiple_of(2)
        || value
            .bytes()
            .any(|byte| !byte.is_ascii_digit() && !(b'a'..=b'f').contains(&byte))
    {
        return Err(Error::Backup("pin control encoding is invalid"));
    }
    (0..value.len())
        .step_by(2)
        .map(|offset| {
            let high = nibble(value.as_bytes()[offset])?;
            let low = nibble(value.as_bytes()[offset + 1])?;
            Ok((high << 4) | low)
        })
        .collect()
}

fn nibble(value: u8) -> Result<u8> {
    match value {
        b'0'..=b'9' => Ok(value - b'0'),
        b'a'..=b'f' => Ok(value - b'a' + 10),
        _ => Err(Error::Backup("pin control encoding is invalid")),
    }
}
