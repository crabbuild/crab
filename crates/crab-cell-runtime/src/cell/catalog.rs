//! Sharded catalog of Cell entries, roles, and proofs.
use bytes::Bytes;
use crab_ltx::CellStorageLayout;
use crab_storage::{ETag, StorageError};
use serde::{Deserialize, Serialize};

use crate::fleet::telemetry::{CatalogReadKind, CellTelemetryHandle};
use crate::identity::encode_hex;
use crate::identity::{ApplicationId, CellId, CellTarget, Digest, NamespaceId, TenantId};
use crate::retry::{Backoff, retry_hint, retryable_storage_error};
use crate::{Error, Result};

// A version-two head carries one page locator per immutable page: a 64-hex
// digest and a 64-hex first Cell id. The full 256-page shard needs about
// 44 KiB, so the bound is one 64 KiB object read rather than the 32 KiB that
// held only digests.
const MAX_HEAD_BYTES: u64 = 64 * 1024;
const MAX_PAGE_BYTES: u64 = 1024 * 1024;
const ENTRIES_PER_PAGE: usize = 256;
const MAX_PAGES: usize = 256;
const MAX_ENTRIES: usize = ENTRIES_PER_PAGE * MAX_PAGES;

/// Namespace behavior pinned when a Cell is first provisioned.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogRole {
    /// The namespace serves repository content.
    Repository,
    /// The namespace serves SQL.
    Sql,
    /// The namespace serves key-value data.
    Kv,
    /// The namespace serves queue messages.
    Queue,
    /// The namespace serves workflow runs.
    Workflow,
    /// The namespace serves blob objects.
    Blob,
    /// The namespace serves cron schedules.
    Cron,
}

/// Immutable identity and bootstrap contract for one cataloged Cell.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogEntry {
    cell: CellId,
    namespace: NamespaceId,
    partition: Vec<u8>,
    role: CatalogRole,
    initial_code: Digest,
    initial_schema: u32,
}

impl CatalogEntry {
    /// Creates a bootstrap entry from a resolved target.
    pub fn new(
        target: &CellTarget,
        role: CatalogRole,
        initial_code: Digest,
        initial_schema: u32,
    ) -> Result<Self> {
        if initial_schema == 0 {
            return Err(Error::Catalog("initial schema is zero"));
        }
        Ok(Self {
            cell: target.cell_id(),
            namespace: target.namespace(),
            partition: target.partition().to_vec(),
            role,
            initial_code,
            initial_schema,
        })
    }

    /// Returns the Cell this entry pins.
    #[must_use]
    pub const fn cell(&self) -> CellId {
        self.cell
    }

    /// Returns the namespace the Cell serves.
    #[must_use]
    pub const fn namespace(&self) -> NamespaceId {
        self.namespace
    }

    /// Returns the partition key that selected the Cell.
    #[must_use]
    pub fn partition(&self) -> &[u8] {
        &self.partition
    }

    /// Returns the namespace role provisioned for the Cell.
    #[must_use]
    pub const fn role(&self) -> CatalogRole {
        self.role
    }

    /// Returns the code digest provisioned for the Cell.
    #[must_use]
    pub const fn initial_code(&self) -> Digest {
        self.initial_code
    }

    /// Returns the schema version provisioned for the Cell.
    #[must_use]
    pub const fn initial_schema(&self) -> u32 {
        self.initial_schema
    }

    fn validate(&self, tenant: TenantId, application: ApplicationId) -> Result<()> {
        if self.initial_schema == 0 || self.partition.len() > 1_024 {
            return Err(Error::Catalog("invalid entry bounds"));
        }
        let target = CellTarget::new(tenant, application, self.namespace, &self.partition)?;
        if target.cell_id() != self.cell {
            return Err(Error::Catalog("entry Cell digest mismatch"));
        }
        Ok(())
    }
}

/// Verified proof that an immutable catalog page is reachable from a shard head.
#[derive(Clone)]
pub struct CatalogProof {
    entry: CatalogEntry,
    revision: u64,
}

/// One verified immutable catalog page from a revision-pinned shard scan.
pub struct CatalogScanPage {
    revision: u64,
    entries: Vec<CatalogProof>,
}

impl CatalogScanPage {
    /// Returns the catalog revision this page belongs to.
    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.revision
    }

    /// Returns the proofs this page carries in key order.
    #[must_use]
    pub fn entries(&self) -> &[CatalogProof] {
        &self.entries
    }
}

/// Stateful bounded scan over one immutable catalog-head snapshot.
pub struct CatalogShardScan {
    catalog: CellCatalog,
    shard: u8,
    revision: u64,
    pages: Vec<CatalogPageRef>,
    next_page: usize,
    previous: Option<CellId>,
}

impl CatalogShardScan {
    /// Returns the pinned catalog revision.
    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.revision
    }

    /// Returns the immutable pages pinned by this shard-head observation.
    #[must_use]
    pub fn page_digests(&self) -> Vec<Digest> {
        self.pages.iter().map(|page| page.digest).collect()
    }

    /// Loads and verifies at most one 256-entry immutable page.
    pub async fn next_page(&mut self) -> Result<Option<CatalogScanPage>> {
        if self.next_page >= self.pages.len() {
            return Ok(None);
        }
        let entries = self
            .catalog
            .load_located_page(self.shard, &self.pages, self.next_page)
            .await?;
        let mut proofs = Vec::with_capacity(entries.len());
        for entry in entries {
            if self
                .previous
                .is_some_and(|previous| previous.as_bytes() >= entry.cell.as_bytes())
            {
                return Err(Error::Catalog("invalid scanned catalog ordering"));
            }
            self.previous = Some(entry.cell);
            proofs.push(CatalogProof {
                entry,
                revision: self.revision,
            });
        }
        self.next_page += 1;
        Ok(Some(CatalogScanPage {
            revision: self.revision,
            entries: proofs,
        }))
    }
}

impl CatalogProof {
    /// Creates a process-local identity proof for an actor-owned resident.
    ///
    /// Revision zero is intentional: this value is never written as catalog
    /// authority or used for retention; the actor lifecycle is the freshness
    /// boundary and the slow route still performs a verified catalog scan.
    pub(crate) fn local(entry: CatalogEntry) -> Self {
        Self { entry, revision: 0 }
    }

    /// Returns the catalog entry this proof carries.
    #[must_use]
    pub const fn entry(&self) -> &CatalogEntry {
        &self.entry
    }

    /// Returns the revision the proof was read at.
    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.revision
    }
}

/// CAS-backed catalog whose immutable pages precede mutable head publication.
#[derive(Clone)]
pub struct CellCatalog {
    layout: CellStorageLayout,
    tenant: TenantId,
    application: ApplicationId,
    telemetry: CellTelemetryHandle,
}

impl CellCatalog {
    /// Binds the catalog to one Cell layout and tenant.
    #[must_use]
    pub fn new(layout: CellStorageLayout, tenant: TenantId) -> Self {
        Self::with_telemetry(layout, tenant, CellTelemetryHandle::default())
    }

    /// Binds the catalog to one Cell layout, tenant, and telemetry sink.
    ///
    /// Routing and due-scan callers pass the node's handle so the metadata
    /// plane's object-store reads appear beside the LTX origin counters.
    #[must_use]
    pub fn with_telemetry(
        layout: CellStorageLayout,
        tenant: TenantId,
        telemetry: CellTelemetryHandle,
    ) -> Self {
        let application = ApplicationId::from_bytes(*layout.application_id());
        Self {
            layout,
            tenant,
            application,
            telemetry,
        }
    }

    /// Returns the application every entry is scoped to.
    #[must_use]
    pub const fn application(&self) -> ApplicationId {
        self.application
    }

    pub(crate) fn matches_identity(
        &self,
        identity: crate::cell::application::ApplicationIdentity,
    ) -> bool {
        self.tenant == identity.tenant() && self.application == identity.application()
    }

    /// Adds one entry with immutable-page-before-head publication ordering.
    pub async fn provision(&self, entry: CatalogEntry) -> Result<CatalogProof> {
        entry.validate(self.tenant, self.application)?;
        let shard = entry.cell.as_bytes()[0];
        let mut backoff = Backoff::default();
        loop {
            let observed = self.load_head(shard).await?;
            let Some(head) = self.insert_entry(shard, observed.as_ref(), &entry).await? else {
                return Ok(CatalogProof {
                    entry,
                    revision: observed.as_ref().map_or(0, |head| head.head.revision),
                });
            };
            let revision = head.revision;
            let encoded = head.encode()?;
            let path = self.layout.catalog_head_path(shard);
            let published = match observed {
                Some(observed) => {
                    self.layout
                        .store()
                        .update(&path, Bytes::from(encoded), observed.token)
                        .await
                }
                None => {
                    self.layout
                        .store()
                        .create_strict_with_etag(&path, Bytes::from(encoded))
                        .await
                }
            };
            match published {
                Ok(_) => return Ok(CatalogProof { entry, revision }),
                Err(error) => {
                    loop {
                        match self.lookup_after_failed_publish(&entry).await {
                            Ok(Some(proof)) => return Ok(proof),
                            Ok(None) => break,
                            Err(Error::Storage(load_error))
                                if retryable_storage_error(&load_error) =>
                            {
                                backoff.wait(retry_hint(&load_error)).await;
                            }
                            Err(load_error) => return Err(load_error),
                        }
                    }
                    if !retryable_storage_error(&error) {
                        return Err(error.into());
                    }
                    backoff.wait(retry_hint(&error)).await;
                }
            }
        }
    }

    /// Loads one entry after checking the head locator and exactly one page.
    ///
    /// The head is a binary-search key list over immutable pages, so a lookup
    /// reads the head and the one page that can hold the Cell. A locator that
    /// disagrees with its page fails closed instead of reporting absence.
    pub async fn lookup(&self, cell: CellId) -> Result<Option<CatalogProof>> {
        let shard = cell.as_bytes()[0];
        let Some(observed) = self.load_head(shard).await? else {
            return Ok(None);
        };
        let Some(index) = observed.head.page_index(cell) else {
            // The Cell id sorts below every provisioned entry in this shard.
            return Ok(None);
        };
        let entries = self
            .load_located_page(shard, &observed.head.pages, index)
            .await?;
        Ok(entries
            .binary_search_by(|entry| entry.cell.as_bytes().cmp(cell.as_bytes()))
            .ok()
            .map(|index| CatalogProof {
                entry: entries[index].clone(),
                revision: observed.head.revision,
            }))
    }

    /// Pins one shard head for bounded immutable-page iteration.
    pub async fn scan_shard(&self, shard: u8) -> Result<CatalogShardScan> {
        let observed = self.load_head(shard).await?;
        let (revision, pages) = observed
            .map(|observed| (observed.head.revision, observed.head.pages))
            .unwrap_or_default();
        Ok(CatalogShardScan {
            catalog: self.clone(),
            shard,
            revision,
            pages,
            next_page: 0,
            previous: None,
        })
    }

    pub(crate) async fn pinned_cells(
        &self,
        shard: u8,
        revision: u64,
        pages: &[Digest],
    ) -> Result<Vec<CellId>> {
        Ok(self.load_pinned_pages(shard, revision, pages).await?.1)
    }

    pub(crate) async fn install_pinned_shard(
        &self,
        shard: u8,
        revision: u64,
        pages: &[Digest],
    ) -> Result<()> {
        let (page_refs, _) = self.load_pinned_pages(shard, revision, pages).await?;
        let observed = self.load_head(shard).await?;
        if revision == 0 {
            return if observed.is_none() {
                Ok(())
            } else {
                Err(Error::Catalog(
                    "empty restored shard conflicts with an existing head",
                ))
            };
        }
        let head = CatalogHead {
            revision,
            pages: page_refs,
        };
        let encoded = head.encode()?;
        let path = self.layout.catalog_head_path(shard);
        match self
            .layout
            .store()
            .create_strict_with_etag(&path, Bytes::from(encoded))
            .await
        {
            Ok(_) => Ok(()),
            Err(create_error) => match self.load_head(shard).await? {
                Some(current)
                    if current.head.revision == revision
                        && current
                            .head
                            .pages
                            .iter()
                            .map(|page| page.digest)
                            .eq(pages.iter().copied()) =>
                {
                    Ok(())
                }
                Some(_) => Err(Error::Catalog(
                    "restored shard conflicts with an existing head",
                )),
                None => Err(create_error.into()),
            },
        }
    }

    async fn lookup_after_failed_publish(
        &self,
        expected: &CatalogEntry,
    ) -> Result<Option<CatalogProof>> {
        match self.lookup(expected.cell).await? {
            Some(proof) if proof.entry == *expected => Ok(Some(proof)),
            Some(_) => Err(Error::CatalogCollision),
            None => Ok(None),
        }
    }

    async fn load_head(&self, shard: u8) -> Result<Option<ObservedHead>> {
        let path = self.layout.catalog_head_path(shard);
        let started = std::time::Instant::now();
        let observed = self
            .layout
            .store()
            .get_with_etag_bounded(&path, MAX_HEAD_BYTES)
            .await;
        let (body, token) = match observed {
            Ok(value) => value,
            Err(StorageError::NotFound { .. }) => {
                self.telemetry
                    .catalog_read(CatalogReadKind::Head, started.elapsed(), true);
                return Ok(None);
            }
            Err(error) => {
                self.telemetry
                    .catalog_read(CatalogReadKind::Head, started.elapsed(), false);
                return Err(error.into());
            }
        };
        self.telemetry
            .catalog_read(CatalogReadKind::Head, started.elapsed(), true);
        Ok(Some(ObservedHead {
            head: CatalogHead::decode(&body)?,
            token,
        }))
    }

    /// Loads every entry of one shard head in key order.
    ///
    /// Provisioning reads the complete shard because it republishes the page
    /// set. Routing uses `lookup`, which reads one page.
    async fn load_entries(&self, shard: u8, head: &CatalogHead) -> Result<Vec<CatalogEntry>> {
        let mut entries = Vec::new();
        for index in 0..head.pages.len() {
            for entry in self.load_located_page(shard, &head.pages, index).await? {
                if entries.last().is_some_and(|previous: &CatalogEntry| {
                    previous.cell.as_bytes() >= entry.cell.as_bytes()
                }) {
                    return Err(Error::Catalog("entries are not globally ordered"));
                }
                entries.push(entry);
            }
        }
        if entries.len() > MAX_ENTRIES {
            return Err(Error::Catalog("head exceeds entry limit"));
        }
        Ok(entries)
    }

    /// Replace only the page that can contain this Cell. The head CAS remains
    /// the serialization point, so a losing writer retries against fresh pages.
    async fn insert_entry(
        &self,
        shard: u8,
        observed: Option<&ObservedHead>,
        entry: &CatalogEntry,
    ) -> Result<Option<CatalogHead>> {
        let Some(observed) = observed else {
            let reference = self.upload_page(std::slice::from_ref(entry)).await?;
            return Ok(Some(CatalogHead {
                revision: 1,
                pages: vec![reference],
            }));
        };
        let mut pages = observed.head.pages.clone();
        let index = observed.head.page_index(entry.cell).unwrap_or(0);
        let mut entries = self.load_located_page(shard, &pages, index).await?;
        match entries.binary_search_by(|value| value.cell.as_bytes().cmp(entry.cell.as_bytes())) {
            Ok(index) if entries[index] == *entry => return Ok(None),
            Ok(_) => return Err(Error::CatalogCollision),
            Err(index) => entries.insert(index, entry.clone()),
        }
        let revision = observed
            .head
            .revision
            .checked_add(1)
            .ok_or(Error::Catalog("head revision overflow"))?;
        if entries.len() <= ENTRIES_PER_PAGE {
            pages[index] = self.upload_page(&entries).await?;
            return Ok(Some(CatalogHead { revision, pages }));
        }
        if pages.len() == MAX_PAGES {
            // Full pages are not guaranteed after earlier splits. Repack once
            // at the head limit before reporting that the shard is full.
            let mut all = self.load_entries(shard, &observed.head).await?;
            let position = match all
                .binary_search_by(|value| value.cell.as_bytes().cmp(entry.cell.as_bytes()))
            {
                Ok(_) => return Err(Error::Catalog("catalog page insertion changed")),
                Err(position) => position,
            };
            all.insert(position, entry.clone());
            if all.len() > MAX_ENTRIES {
                return Err(Error::CatalogFull);
            }
            return self.upload_pages(revision, &all).await.map(Some);
        }
        let right = entries.split_off(entries.len() / 2);
        let left = self.upload_page(&entries).await?;
        let right = self.upload_page(&right).await?;
        pages.splice(index..=index, [left, right]);
        Ok(Some(CatalogHead { revision, pages }))
    }

    /// Loads one immutable page and proves it belongs where the locator says.
    ///
    /// The checks make a head that disagrees with its pages fail closed: the
    /// page must open at the located first Cell id, stay inside the shard,
    /// remain ordered, and end below the next locator key.
    async fn load_located_page(
        &self,
        shard: u8,
        pages: &[CatalogPageRef],
        index: usize,
    ) -> Result<Vec<CatalogEntry>> {
        let reference = pages
            .get(index)
            .ok_or(Error::Catalog("invalid head page count"))?;
        if reference.first.as_bytes()[0] != shard {
            return Err(Error::Catalog("catalog page locator names another shard"));
        }
        let entries = self.load_page(reference.digest).await?;
        let mut previous: Option<&[u8]> = None;
        for entry in &entries {
            entry.validate(self.tenant, self.application)?;
            if entry.cell.as_bytes()[0] != shard
                || previous.is_some_and(|value| value >= entry.cell.as_bytes().as_slice())
            {
                return Err(Error::Catalog("invalid located catalog ordering or shard"));
            }
            previous = Some(entry.cell.as_bytes());
        }
        if entries
            .first()
            .is_none_or(|entry| entry.cell != reference.first)
        {
            return Err(Error::Catalog(
                "catalog page locator disagrees with its page",
            ));
        }
        if let Some(next) = pages.get(index + 1)
            && entries
                .last()
                .is_some_and(|entry| entry.cell.as_bytes() >= next.first.as_bytes())
        {
            return Err(Error::Catalog(
                "catalog page locator disagrees with its page",
            ));
        }
        Ok(entries)
    }

    /// Loads a pinned page list and returns its locators and Cells.
    ///
    /// Backup restore replays a manifest that names page digests only, so the
    /// locators are rebuilt from the verified pages here.
    async fn load_pinned_pages(
        &self,
        shard: u8,
        revision: u64,
        pages: &[Digest],
    ) -> Result<(Vec<CatalogPageRef>, Vec<CellId>)> {
        if pages.len() > MAX_PAGES || (revision == 0) != pages.is_empty() {
            return Err(Error::Catalog("invalid pinned catalog head"));
        }
        let mut unique = std::collections::HashSet::with_capacity(pages.len());
        if pages
            .iter()
            .any(|digest| !unique.insert(*digest.as_bytes()))
        {
            return Err(Error::Catalog("duplicate pinned catalog page"));
        }
        let mut page_refs = Vec::with_capacity(pages.len());
        let mut cells = Vec::new();
        for digest in pages {
            let entries = self.load_page(*digest).await?;
            let Some(first) = entries.first().map(|entry| entry.cell) else {
                return Err(Error::Catalog("invalid page entry count"));
            };
            for entry in &entries {
                entry.validate(self.tenant, self.application)?;
                if entry.cell.as_bytes()[0] != shard
                    || cells.last().is_some_and(|previous: &CellId| {
                        previous.as_bytes() >= entry.cell.as_bytes()
                    })
                {
                    return Err(Error::Catalog("invalid pinned catalog ordering or shard"));
                }
                cells.push(entry.cell);
            }
            page_refs.push(CatalogPageRef {
                digest: *digest,
                first,
            });
        }
        if cells.len() > MAX_ENTRIES {
            return Err(Error::Catalog("pinned catalog exceeds entry limit"));
        }
        Ok((page_refs, cells))
    }

    async fn load_page(&self, digest: Digest) -> Result<Vec<CatalogEntry>> {
        let path = self.layout.catalog_object_path(digest.as_bytes());
        let started = std::time::Instant::now();
        let observed = self
            .layout
            .store()
            .get_with_etag_bounded(&path, MAX_PAGE_BYTES)
            .await;
        let (body, _) = match observed {
            Ok(value) => value,
            Err(error) => {
                self.telemetry
                    .catalog_read(CatalogReadKind::Page, started.elapsed(), false);
                return Err(error.into());
            }
        };
        self.telemetry
            .catalog_read(CatalogReadKind::Page, started.elapsed(), true);
        if blake3::hash(&body).as_bytes() != digest.as_bytes() {
            return Err(Error::Catalog("page digest mismatch"));
        }
        let page = CatalogPage::decode(&body)?;
        if page.entries.is_empty() || page.entries.len() > ENTRIES_PER_PAGE {
            return Err(Error::Catalog("invalid page entry count"));
        }
        Ok(page.entries)
    }

    async fn upload_pages(&self, revision: u64, entries: &[CatalogEntry]) -> Result<CatalogHead> {
        let mut pages = Vec::new();
        for entries in entries.chunks(ENTRIES_PER_PAGE) {
            pages.push(self.upload_page(entries).await?);
        }
        if pages.is_empty() || pages.len() > MAX_PAGES {
            return Err(Error::Catalog("invalid head page count"));
        }
        Ok(CatalogHead { revision, pages })
    }

    async fn upload_page(&self, entries: &[CatalogEntry]) -> Result<CatalogPageRef> {
        let first = entries
            .first()
            .ok_or(Error::Catalog("empty catalog page"))?
            .cell;
        let encoded = CatalogPage {
            entries: entries.to_vec(),
        }
        .encode()?;
        if encoded.len() as u64 > MAX_PAGE_BYTES {
            return Err(Error::Catalog("encoded page exceeds 1 MiB"));
        }
        let digest = Digest::from_bytes(*blake3::hash(&encoded).as_bytes());
        self.layout
            .store()
            .put(
                &self.layout.catalog_object_path(digest.as_bytes()),
                Bytes::from(encoded),
            )
            .await?;
        Ok(CatalogPageRef { digest, first })
    }
}

mod codec;

use codec::*;
