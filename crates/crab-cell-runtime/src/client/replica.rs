//! Explicit snapshot read path, gated against current Cell authority.

use std::{
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

use crab_ltx::{CellReplica, ReadOnlyRoot};
use tokio::sync::Semaphore;

use super::*;
use crate::control::authority::CellAuthority;
use crate::control::{Control, ControlState, Owner};
use crate::node::NodeDirectory;

const QUERY_DEADLINE: Duration = Duration::from_secs(5);

/// One immutable replica snapshot that serves explicit, position-tagged reads.
///
/// The caller owns admission, routing, and authorization. Every successful
/// query checks authoritative control and the owner's live session after SQL
/// execution; a stale epoch or unavailable authority releases no output.
#[derive(Clone)]
pub struct CellReadReplica {
    registry: Arc<Registry>,
    authority: CellAuthority,
    directory: NodeDirectory,
    target: CellTarget,
    expected: CellDescription,
    owner: Owner,
    epoch: u64,
    view: Arc<ReadOnlyRoot>,
    query_gate: Arc<Semaphore>,
}

impl CellReadReplica {
    /// Restores the exact S3 root currently named by one live serving owner.
    ///
    /// The caller supplies a fresh private destination. Source Cell control
    /// must stay serving under the same owner epoch through installation.
    pub async fn open(
        registry: Arc<Registry>,
        authority: CellAuthority,
        directory: NodeDirectory,
        replica: CellReplica,
        target: CellTarget,
        destination: &Path,
    ) -> Result<Self> {
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
        let opened = Self {
            registry,
            authority,
            directory,
            target,
            expected,
            owner,
            epoch: control.epoch,
            view,
            query_gate: Arc::new(Semaphore::new(1)),
        };
        opened.confirm_authority().await?;
        Ok(opened)
    }

    /// Returns the exact snapshot position this reader serves.
    #[must_use]
    pub fn receipt(&self) -> Receipt {
        receipt(self.expected, self.view.root().commit_sequence)
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
        validate_minimum(self.expected, minimum)?;
        let observed = self.receipt();
        if minimum.is_some_and(|minimum| observed.commit_sequence < minimum.commit_sequence) {
            return Err(Error::Command("replica is behind requested receipt"));
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
        let interrupt = self.view.connection()?.get_interrupt_handle();
        let view = Arc::clone(&self.view);
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
        tokio::time::timeout_at(deadline.into(), self.confirm_authority())
            .await
            .map_err(|_| Error::Deadline)??;
        Ok(Observed {
            output: decode_wire(&result, operation.output_limit)?,
            receipt: observed,
        })
    }

    async fn confirm_authority(&self) -> Result<()> {
        let current = self
            .authority
            .load(self.expected.cell)
            .await?
            .ok_or(Error::Fenced)?;
        let current = current.value();
        if !self.same_owner_and_code(current) {
            return Err(Error::Fenced);
        }
        if !self
            .directory
            .is_live(self.owner.session, unix_time_ms()?)
            .await?
        {
            return Err(Error::Fenced);
        }
        Ok(())
    }

    fn same_owner_and_code(&self, current: &Control) -> bool {
        current.state == ControlState::Serving
            && current.recovery.is_none()
            && current.epoch == self.epoch
            && current.incarnation == self.expected.incarnation
            && current.code == self.expected.code
            && current.schema == self.expected.schema
            && current.owner.as_ref() == Some(&self.owner)
            && current
                .root
                .as_ref()
                .is_some_and(|root| root.commit_sequence >= self.view.root().commit_sequence)
    }
}
