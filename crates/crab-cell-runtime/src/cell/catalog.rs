use bytes::Bytes;
use crab_ltx::CellStorageLayout;
use crab_storage::{ETag, StorageError};
use serde::{Deserialize, Serialize};

use crate::identity::encode_hex;
use crate::identity::{ApplicationId, CellId, CellTarget, Digest, NamespaceId, TenantId};
use crate::{Error, Result};

const MAX_HEAD_BYTES: u64 = 32 * 1024;
const MAX_PAGE_BYTES: u64 = 1024 * 1024;
const ENTRIES_PER_PAGE: usize = 256;
const MAX_PAGES: usize = 256;
const MAX_ENTRIES: usize = ENTRIES_PER_PAGE * MAX_PAGES;

/// Namespace behavior pinned when a Cell is first provisioned.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogRole {
    Repository,
    Sql,
    Kv,
    Queue,
    Workflow,
    Blob,
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

    #[must_use]
    pub const fn cell(&self) -> CellId {
        self.cell
    }

    #[must_use]
    pub const fn namespace(&self) -> NamespaceId {
        self.namespace
    }

    #[must_use]
    pub fn partition(&self) -> &[u8] {
        &self.partition
    }

    #[must_use]
    pub const fn role(&self) -> CatalogRole {
        self.role
    }

    #[must_use]
    pub const fn initial_code(&self) -> Digest {
        self.initial_code
    }

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
    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.revision
    }

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
    pages: Vec<Digest>,
    next_page: usize,
    previous: Option<CellId>,
}

impl CatalogShardScan {
    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.revision
    }

    /// Returns the immutable pages pinned by this shard-head observation.
    #[must_use]
    pub fn page_digests(&self) -> &[Digest] {
        &self.pages
    }

    /// Loads and verifies at most one 256-entry immutable page.
    pub async fn next_page(&mut self) -> Result<Option<CatalogScanPage>> {
        let Some(digest) = self.pages.get(self.next_page).copied() else {
            return Ok(None);
        };
        let entries = self.catalog.load_page(digest).await?;
        let mut proofs = Vec::with_capacity(entries.len());
        for entry in entries {
            entry.validate(self.catalog.tenant, self.catalog.application)?;
            if entry.cell.as_bytes()[0] != self.shard
                || self
                    .previous
                    .is_some_and(|previous| previous.as_bytes() >= entry.cell.as_bytes())
            {
                return Err(Error::Catalog("invalid scanned catalog ordering or shard"));
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

    #[must_use]
    pub const fn entry(&self) -> &CatalogEntry {
        &self.entry
    }

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
}

impl CellCatalog {
    #[must_use]
    pub fn new(layout: CellStorageLayout, tenant: TenantId) -> Self {
        let application = ApplicationId::from_bytes(*layout.application_id());
        Self {
            layout,
            tenant,
            application,
        }
    }

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
        let mut backoff = CatalogBackoff::default();
        loop {
            let observed = self.load_head(shard).await?;
            let mut entries = match &observed {
                Some(observed) => self.load_entries(&observed.head).await?,
                None => Vec::new(),
            };
            match entries.binary_search_by(|value| value.cell.as_bytes().cmp(entry.cell.as_bytes()))
            {
                Ok(index) if entries[index] == entry => {
                    return Ok(CatalogProof {
                        entry,
                        revision: observed
                            .as_ref()
                            .map_or(0, |observed| observed.head.revision),
                    });
                }
                Ok(_) => return Err(Error::CatalogCollision),
                Err(index) => entries.insert(index, entry.clone()),
            }
            if entries.len() > MAX_ENTRIES {
                return Err(Error::CatalogFull);
            }
            let revision = match &observed {
                Some(observed) => observed
                    .head
                    .revision
                    .checked_add(1)
                    .ok_or(Error::Catalog("head revision overflow"))?,
                None => 1,
            };
            let head = self.upload_pages(revision, &entries).await?;
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

    /// Loads one entry only after checking the head and every referenced page.
    pub async fn lookup(&self, cell: CellId) -> Result<Option<CatalogProof>> {
        let Some(observed) = self.load_head(cell.as_bytes()[0]).await? else {
            return Ok(None);
        };
        let entries = self.load_entries(&observed.head).await?;
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
        let mut cells = Vec::new();
        for digest in pages {
            for entry in self.load_page(*digest).await? {
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
        }
        if cells.len() > MAX_ENTRIES {
            return Err(Error::Catalog("pinned catalog exceeds entry limit"));
        }
        Ok(cells)
    }

    pub(crate) async fn install_pinned_shard(
        &self,
        shard: u8,
        revision: u64,
        pages: &[Digest],
    ) -> Result<()> {
        self.pinned_cells(shard, revision, pages).await?;
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
            pages: pages.to_vec(),
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
                    if current.head.revision == revision && current.head.pages == pages =>
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
        let (body, token) = match self
            .layout
            .store()
            .get_with_etag_bounded(&path, MAX_HEAD_BYTES)
            .await
        {
            Ok(value) => value,
            Err(StorageError::NotFound { .. }) => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        Ok(Some(ObservedHead {
            head: CatalogHead::decode(&body)?,
            token,
        }))
    }

    async fn load_entries(&self, head: &CatalogHead) -> Result<Vec<CatalogEntry>> {
        let mut entries = Vec::new();
        for digest in &head.pages {
            for entry in self.load_page(*digest).await? {
                entry.validate(self.tenant, self.application)?;
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

    async fn load_page(&self, digest: Digest) -> Result<Vec<CatalogEntry>> {
        let path = self.layout.catalog_object_path(digest.as_bytes());
        let (body, _) = self
            .layout
            .store()
            .get_with_etag_bounded(&path, MAX_PAGE_BYTES)
            .await?;
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
            pages.push(digest);
        }
        if pages.is_empty() || pages.len() > MAX_PAGES {
            return Err(Error::Catalog("invalid head page count"));
        }
        Ok(CatalogHead { revision, pages })
    }
}

mod backoff;
mod codec;

use backoff::*;
use codec::*;
