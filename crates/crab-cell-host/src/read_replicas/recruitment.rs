//! Owner-side recruitment for every compiled application namespace.

use super::*;
use crab_cell_runtime::{
    cell::{application::ApplicationIdentity, catalog::CatalogEntry},
    client::CellDescription,
    peer::ReplicaPeerClient,
};
use futures_util::{StreamExt, stream};
use tokio::sync::broadcast;

const ACTIVATION_CONCURRENCY: usize = 16;

/// Recruits selected readers for the locally owned Cells of one application.
///
/// Activation is advisory. Each receiver independently checks current owner,
/// policy, membership, and resource admission before opening a read snapshot.
#[derive(Clone)]
pub struct ReadReplicaRecruiter {
    readers: ReadReplicaManager,
    identity: ApplicationIdentity,
    peer: ReplicaPeerClient,
    cursor: Arc<Mutex<usize>>,
}

impl ReadReplicaRecruiter {
    /// Binds owner recruitment to an existing manager and authenticated transport.
    pub fn new(
        readers: ReadReplicaManager,
        identity: ApplicationIdentity,
        peer: ReplicaPeerClient,
    ) -> Result<Self> {
        if identity.application().as_bytes() != readers.layout.application_id() {
            return Err(Error::Control(
                "read recruitment application does not match storage",
            ));
        }
        Ok(Self {
            readers,
            identity,
            peer,
            cursor: Arc::new(Mutex::new(0)),
        })
    }

    /// Reconciles one authorized target within a bounded, cancellable owner check.
    pub async fn reconcile(&self, target: CellTarget) -> Result<()> {
        self.readers.ensure_open()?;
        if target.tenant() != self.identity.tenant()
            || target.application() != self.identity.application()
            || self
                .readers
                .registry
                .namespace_contract(target.namespace())
                .is_none()
        {
            return Err(Error::PeerAuthorization(
                "read recruitment target is outside the application",
            ));
        }
        tokio::select! {
            () = self.readers.closed.cancelled() => Err(Error::RuntimeClosed),
            result = tokio::time::timeout(RECONCILE_DEADLINE, self.reconcile_open(target)) => {
                result.map_err(|_| Error::Deadline)?
            }
        }
    }

    async fn reconcile_open(&self, target: CellTarget) -> Result<()> {
        let cell = target.cell_id();
        let Some(policy) = self.readers.policy.load(cell).await? else {
            return Ok(());
        };
        let policy = policy.value();
        if policy.desired_readers() == 0 {
            return Ok(());
        }
        let Some(control) = self.readers.authority.load(cell).await? else {
            return Ok(());
        };
        let control = control.value();
        if control.state != ControlState::Serving
            || control.recovery.is_some()
            || control
                .owner
                .as_ref()
                .is_none_or(|owner| owner.session != self.readers.session)
            || policy.incarnation() != control.incarnation
        {
            return Ok(());
        }
        let expected = CellDescription {
            cell,
            incarnation: control.incarnation,
            code: control.code,
            schema: control.schema,
        };
        let selected = self
            .readers
            .directory
            .select_readers(
                cell,
                self.readers.session,
                control.code,
                usize::from(policy.desired_readers()),
                now_ms()?,
                MAX_LIVE_NODES,
            )
            .await?;
        // A slow selected node must not prevent healthy nodes from opening.
        // Each dispatch reloads the exact boot session after waiting for fanout.
        let attempts = stream::iter(selected)
            .map(|node| {
                self.peer
                    .activate(&target, &self.readers.directory, node, expected)
            })
            .buffer_unordered(ACTIVATION_CONCURRENCY);
        tokio::pin!(attempts);
        while let Some(result) = attempts.next().await {
            if let Err(error) = result {
                tracing::warn!(?cell, error = %error, "read replica activation hint failed");
            }
        }
        Ok(())
    }

    /// Runs one bounded pass over local owner Cells, retaining fair progress across retries.
    pub async fn reconcile_active(&self) -> Result<()> {
        self.readers.ensure_open()?;
        tokio::select! {
            () = self.readers.closed.cancelled() => Err(Error::RuntimeClosed),
            result = tokio::time::timeout(RECONCILE_DEADLINE, self.reconcile_active_open()) => {
                result.map_err(|_| Error::Deadline)?
            }
        }
    }

    async fn reconcile_active_open(&self) -> Result<()> {
        let mut cursor = self.cursor.lock().await;
        let mut entries = self.readers.runtime.active_catalog_entries().await?;
        entries.sort_by_key(|entry| *entry.cell().as_bytes());
        if entries.is_empty() {
            *cursor = 0;
            return Ok(());
        }
        for _ in 0..entries.len().min(RECONCILE_BATCH) {
            let index = *cursor % entries.len();
            let entry = &entries[index];
            // Advance before I/O; an interrupted or corrupt Cell cannot hold
            // every later Cell behind the same bounded pass indefinitely.
            *cursor = (index + 1) % entries.len();
            let result = self.reconcile_entry(entry).await;
            if let Err(error) = result {
                tracing::warn!(cell = ?entry.cell(), error = %error, "read replica recruitment failed");
            }
        }
        Ok(())
    }

    async fn reconcile_entry(&self, entry: &CatalogEntry) -> Result<()> {
        let target = CellTarget::new(
            self.identity.tenant(),
            self.identity.application(),
            entry.namespace(),
            entry.partition(),
        )?;
        if target.cell_id() != entry.cell() {
            return Ok(());
        }
        self.reconcile(target).await
    }

    async fn reconcile_published(
        &self,
        entry: CatalogEntry,
        publications: &mut broadcast::Receiver<CatalogEntry>,
    ) -> Result<()> {
        // Bound both retained notifications and work per pass. Repeated writes
        // coalesce to one fresh authority read; periodic scans repair lost hints.
        let mut entries = HashMap::from([(entry.cell(), entry)]);
        for _ in 1..RECONCILE_BATCH {
            match publications.try_recv() {
                Ok(entry) => {
                    entries.insert(entry.cell(), entry);
                }
                Err(_) => break,
            }
        }
        for entry in entries.values() {
            if let Err(error) = self.reconcile_entry(entry).await {
                tracing::warn!(cell = ?entry.cell(), error = %error, "published Cell reader hint failed");
            }
        }
        Ok(())
    }

    pub(crate) async fn run(&self, cancellation: CancellationToken) -> Result<()> {
        let mut publications = self.readers.runtime.subscribe_publications();
        let mut tick = tokio::time::interval(RECONCILE_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            let published = tokio::select! {
                () = cancellation.cancelled() => return Ok(()),
                () = self.readers.closed.cancelled() => return Ok(()),
                _ = tick.tick() => None,
                published = publications.recv() => match published {
                    Ok(entry) => Some(entry),
                    Err(broadcast::error::RecvError::Lagged(_)) => None,
                    Err(broadcast::error::RecvError::Closed) => return Ok(()),
                },
            };
            let reconcile = async {
                match published {
                    Some(entry) => tokio::time::timeout(
                        RECONCILE_DEADLINE,
                        self.reconcile_published(entry, &mut publications),
                    )
                    .await
                    .map_err(|_| Error::Deadline)?,
                    None => self.reconcile_active().await,
                }
            };
            tokio::select! {
                () = cancellation.cancelled() => return Ok(()),
                () = self.readers.closed.cancelled() => return Ok(()),
                result = reconcile => {
                    if let Err(error) = result {
                        tracing::warn!(error = %error, "read replica recruitment pass failed");
                    }
                }
            }
        }
    }
}
