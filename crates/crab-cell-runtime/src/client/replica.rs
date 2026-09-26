//! Explicit snapshot read path, gated against current Cell authority.

use std::{
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

use crab_ltx::{CellReplica, ReadOnlyRoot};
use tokio::sync::{Mutex, RwLock, Semaphore};

use super::*;
use crate::cell::actor::CellRuntime;
use crate::control::authority::CellAuthority;
use crate::control::{Control, ControlState, Owner};
use crate::fleet::resource::ResourceReservation;
use crate::node::NodeDirectory;

const QUERY_DEADLINE: Duration = Duration::from_secs(5);

/// One immutable replica snapshot that serves explicit, position-tagged reads.
///
/// The caller owns routing and authorization. Every successful
/// query checks authoritative control and the owner's live session after SQL
/// execution; a stale epoch or unavailable authority releases no output.
#[derive(Clone)]
pub struct CellReadReplica {
    runtime: CellRuntime,
    registry: Arc<Registry>,
    authority: CellAuthority,
    directory: NodeDirectory,
    replica: CellReplica,
    target: CellTarget,
    expected: CellDescription,
    snapshot: Arc<RwLock<ReplicaSnapshot>>,
    refresh_gate: Arc<Mutex<()>>,
    query_gate: Arc<Semaphore>,
}

#[derive(Clone)]
struct ReplicaSnapshot {
    owner: Owner,
    epoch: u64,
    view: Arc<ReadOnlyRoot>,
    _admission: Arc<ResourceReservation>,
}

impl CellReadReplica {
    /// Restores the exact S3 root currently named by one live serving owner.
    ///
    /// The caller supplies a fresh private destination and an admitting node
    /// runtime. Source Cell control must stay serving under the same owner
    /// epoch through installation.
    pub async fn open(
        runtime: CellRuntime,
        registry: Arc<Registry>,
        authority: CellAuthority,
        directory: NodeDirectory,
        replica: CellReplica,
        target: CellTarget,
        destination: &Path,
    ) -> Result<Self> {
        let admission = Arc::new(runtime.reserve_read_view()?);
        let replica = runtime.replica_for_read(replica);
        let cell = target.cell_id();
        let observed = authority.load(cell).await?.ok_or(Error::CellNotActive)?;
        let control = observed.value();
        let owner = control.owner.as_ref().ok_or(Error::Fenced)?.clone();
        if control.state != ControlState::Serving || control.recovery.is_some() {
            return Err(Error::Fenced);
        }
        let (module, descriptor) = registry
            .namespace_contract(target.namespace())
            .ok_or(Error::Registry("replica namespace is not registered"))?;
        if descriptor.role != CatalogRole::Repository
            || !registry.supports_module_code(module, control.code, control.schema)
        {
            return Err(Error::Registry("replica module or code is unsupported"));
        }
        if !directory.is_live(owner.session, unix_time_ms()?).await? {
            return Err(Error::Fenced);
        }
        let root = control.ltx_root().ok_or(Error::Fenced)?;
        let verified = replica.open_root(&root).await?;
        if verified.schema() != control.schema {
            return Err(Error::Fenced);
        }
        let expected = CellDescription {
            cell,
            incarnation: control.incarnation,
            code: control.code,
            schema: control.schema,
        };
        let view = Arc::new(verified.open_read_only(destination).await?);
        let snapshot = ReplicaSnapshot {
            owner,
            epoch: control.epoch,
            view,
            _admission: admission,
        };
        let opened = Self {
            runtime,
            registry,
            authority,
            directory,
            replica,
            target,
            expected,
            snapshot: Arc::new(RwLock::new(snapshot.clone())),
            refresh_gate: Arc::new(Mutex::new(())),
            query_gate: Arc::new(Semaphore::new(1)),
        };
        opened.confirm_authority(&snapshot).await?;
        Ok(opened)
    }

    /// Returns the exact snapshot position this reader serves.
    #[must_use]
    pub async fn receipt(&self) -> Receipt {
        let snapshot = self.snapshot.read().await;
        self.snapshot_receipt(&snapshot)
    }

    /// Installs a newer exact root without disrupting queries using the old view.
    ///
    /// The destination must be fresh and private. Concurrent refreshes are
    /// serialized; a failed or stale refresh leaves the serving view intact.
    pub async fn refresh(&self, destination: &Path) -> Result<Receipt> {
        self.runtime.ensure_running()?;
        let _refresh = self.refresh_gate.lock().await;
        let current = self.snapshot.read().await.clone();
        self.confirm_authority(&current).await?;
        let observed = self
            .authority
            .load(self.expected.cell)
            .await?
            .ok_or(Error::Fenced)?;
        let control = observed.value();
        if !self.same_owner_and_code(control, &current) {
            return Err(Error::Fenced);
        }
        let root = control.ltx_root().ok_or(Error::Fenced)?;
        if root.commit_sequence < current.view.root().commit_sequence {
            return Err(Error::Fenced);
        }
        if root == current.view.root() {
            return Ok(self.snapshot_receipt(&current));
        }
        let admission = Arc::new(self.runtime.reserve_read_view()?);
        let verified = self.replica.open_root(&root).await?;
        if verified.schema() != self.expected.schema {
            return Err(Error::Fenced);
        }
        let replacement = ReplicaSnapshot {
            owner: current.owner.clone(),
            epoch: current.epoch,
            view: Arc::new(verified.open_read_only(destination).await?),
            _admission: admission,
        };
        self.confirm_authority(&replacement).await?;
        let receipt = self.snapshot_receipt(&replacement);
        *self.snapshot.write().await = replacement;
        Ok(receipt)
    }

    /// Executes one compiled typed query against this read-only snapshot.
    ///
    /// A minimum newer than this view fails rather than returning an older
    /// value. Authority is checked after SQL before any result is released.
    pub async fn query<Q: Query>(
        &self,
        minimum: Option<Receipt>,
        input: Q::Input,
    ) -> Result<Observed<Q::Output>> {
        self.runtime.ensure_running()?;
        validate_minimum(self.expected, minimum)?;
        let snapshot = self.snapshot.read().await.clone();
        let observed = self.snapshot_receipt(&snapshot);
        if let Some(minimum) =
            minimum.filter(|minimum| observed.commit_sequence < minimum.commit_sequence)
        {
            return Err(Error::ReplicaBehind {
                observed_sequence: observed.commit_sequence,
                minimum_sequence: minimum.commit_sequence,
            });
        }
        let operation = self.registry.query_contract::<Q>(self.target.namespace())?;
        validate_description(&self.registry, Q::MODULE, self.expected, operation)?;
        let input = encode_wire(&input, operation.input_limit)?;
        let deadline = Instant::now() + QUERY_DEADLINE;
        let permit = tokio::time::timeout_at(
            deadline.into(),
            Arc::clone(&self.query_gate).acquire_owned(),
        )
        .await
        .map_err(|_| Error::Deadline)?
        .map_err(|_| Error::RuntimeClosed)?;
        let interrupt = snapshot.view.connection()?.get_interrupt_handle();
        let view = Arc::clone(&snapshot.view);
        let registry = Arc::clone(&self.registry);
        let cell = self.expected.cell;
        let schema = self.expected.schema;
        let sequence = observed.commit_sequence;
        let now_ms = unix_time_ms()?;
        let mut task = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let connection = view.connection()?;
            if local::current_sequence(&connection)? != sequence {
                return Err(Error::Fenced);
            }
            registry.execute_query(
                &connection,
                QueryInvocation {
                    module: Q::MODULE,
                    operation_id: Q::ID,
                    codec_version: Q::CODEC_VERSION,
                    schema,
                    cell,
                    commit_sequence: sequence,
                    now_ms,
                    input: &input,
                },
            )
        });
        let result = match tokio::time::timeout_at(deadline.into(), &mut task).await {
            Ok(result) => result.map_err(Error::WorkerJoin)?,
            Err(_) => {
                interrupt.interrupt();
                let _ = task.await;
                return Err(Error::Deadline);
            }
        }?;
        tokio::time::timeout_at(deadline.into(), self.confirm_authority(&snapshot))
            .await
            .map_err(|_| Error::Deadline)??;
        self.runtime.ensure_running()?;
        Ok(Observed {
            output: decode_wire(&result, operation.output_limit)?,
            receipt: observed,
        })
    }

    fn snapshot_receipt(&self, snapshot: &ReplicaSnapshot) -> Receipt {
        receipt(self.expected, snapshot.view.root().commit_sequence)
    }

    async fn confirm_authority(&self, snapshot: &ReplicaSnapshot) -> Result<()> {
        let current = self
            .authority
            .load(self.expected.cell)
            .await?
            .ok_or(Error::Fenced)?;
        let current = current.value();
        if !self.same_owner_and_code(current, snapshot) {
            return Err(Error::Fenced);
        }
        if !self
            .directory
            .is_live(snapshot.owner.session, unix_time_ms()?)
            .await?
        {
            return Err(Error::Fenced);
        }
        Ok(())
    }

    fn same_owner_and_code(&self, current: &Control, snapshot: &ReplicaSnapshot) -> bool {
        current.state == ControlState::Serving
            && current.recovery.is_none()
            && current.epoch == snapshot.epoch
            && current.incarnation == self.expected.incarnation
            && current.code == self.expected.code
            && current.schema == self.expected.schema
            && current.owner.as_ref() == Some(&snapshot.owner)
            && current
                .root
                .as_ref()
                .is_some_and(|root| root.commit_sequence >= snapshot.view.root().commit_sequence)
    }
}
