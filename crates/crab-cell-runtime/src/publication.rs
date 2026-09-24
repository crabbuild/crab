//! Immutable root preparation, authority CAS, and result release.
use crate::cell::executor::{CellExecutor, StoredOutcome};
use crate::control::Transition;
use crate::control::authority::{CellAuthority, VersionedControl};
use crate::fleet::telemetry::DurabilitySubmissionOutcome;
use crate::identity::{ApplicationId, encode_hex};
use crate::node::durability::NodeDurability;
use crate::node::log::CommitTicket;
use crate::node::log_shipper::NodeLogSubmission;
use crate::{Error, Result};

const MAX_RETRY_DELAY_MS: u64 = 1_000;
const COMPACTION_CHECK_INTERVAL: u8 = 8;
const COMPACTION_DEBT_SEGMENTS: usize = 32;
const MAX_COMPACTION_CASCADE: usize = 9;
const RENEW_INTERVAL: std::time::Duration = std::time::Duration::from_secs(3);
const SELF_FENCE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

pub(crate) type NodeDurabilityBinding = (ApplicationId, std::sync::Arc<NodeDurability>);
pub(crate) type NodeDurabilitySlot =
    std::sync::Arc<std::sync::RwLock<Option<NodeDurabilityBinding>>>;

#[derive(Clone)]
pub(crate) struct CellDurabilitySubmitter {
    cell: crate::CellId,
    incarnation: crate::identity::IncarnationId,
    epoch: u64,
    node_lease: Option<crate::NodeLeaseGuard>,
    node_durability: Option<NodeDurabilitySlot>,
    telemetry: crate::fleet::telemetry::CellTelemetryHandle,
}

/// Coordinates immutable preparation, authority CAS and result release.
///
/// A failed CAS response is reconciled against origin before returning. Exact
/// prepared-root equality proves success; a pure renewal can be retried without
/// rerunning SQL. Any ownership or root divergence fences local admission.
pub struct CellPublisher {
    replica: crab_ltx::CellReplica,
    authority: CellAuthority,
    observed: VersionedControl,
    scratch_directory: std::path::PathBuf,
    segment_count: Option<usize>,
    appends_since_compaction_check: u8,
    renew_at: std::time::Instant,
    node_lease: Option<crate::NodeLeaseGuard>,
    node_durability: Option<NodeDurabilitySlot>,
    telemetry: crate::fleet::telemetry::CellTelemetryHandle,
}

impl CellPublisher {
    /// Creates an owner-bound publisher with a private compaction scratch directory.
    ///
    /// The directory must exist and remain private to this Cell activation.
    #[must_use]
    pub fn new(
        replica: crab_ltx::CellReplica,
        authority: CellAuthority,
        observed: VersionedControl,
        scratch_directory: std::path::PathBuf,
    ) -> Self {
        Self {
            replica,
            authority,
            observed,
            scratch_directory,
            segment_count: None,
            appends_since_compaction_check: 0,
            renew_at: std::time::Instant::now() + RENEW_INTERVAL,
            node_lease: None,
            node_durability: None,
            telemetry: crate::fleet::telemetry::CellTelemetryHandle::default(),
        }
    }

    pub(crate) fn with_node_lease(mut self, node_lease: crate::NodeLeaseGuard) -> Self {
        self.node_lease = Some(node_lease);
        self
    }

    pub(crate) fn with_node_durability_slot(mut self, durability: NodeDurabilitySlot) -> Self {
        self.node_durability = Some(durability);
        self
    }

    pub(crate) fn with_telemetry(
        mut self,
        telemetry: crate::fleet::telemetry::CellTelemetryHandle,
    ) -> Self {
        self.telemetry = telemetry;
        self
    }

    pub(crate) async fn submit_migration_durability(
        &self,
        pending: &crate::cell::executor::PendingMigration,
    ) -> Result<Option<PendingDurability>> {
        self.durability_submitter()
            .submit(pending.commit_sequence(), pending.cuts())
            .await
    }

    pub(crate) fn durability_submitter(&self) -> CellDurabilitySubmitter {
        let control = self.observed.value();
        CellDurabilitySubmitter {
            cell: control.cell,
            incarnation: control.incarnation,
            epoch: control.epoch,
            node_lease: self.node_lease.clone(),
            node_durability: self.node_durability.clone(),
            telemetry: self.telemetry.clone(),
        }
    }

    pub(crate) fn record_object_proof(&self, waited: std::time::Duration) {
        self.telemetry
            .durability_proof(crate::node::log::DurabilitySource::Object, waited);
    }

    #[must_use]
    pub fn control(&self) -> &VersionedControl {
        &self.observed
    }

    pub(crate) fn renewal_due(&self, now: std::time::Instant) -> bool {
        now >= self.renew_at
    }

    pub(crate) fn renewal_at(&self) -> std::time::Instant {
        self.renew_at
    }

    pub(crate) fn compaction_due(&self) -> bool {
        self.observed.value().ltx_root().is_some()
            && self.appends_since_compaction_check >= COMPACTION_CHECK_INTERVAL
    }

    /// Runs at most one promotion while the actor owns the publisher token.
    /// Retryable preparation failures leave the debt for a later quiet period.
    pub(crate) async fn compact_one_quiet(&mut self) -> Result<Option<bool>> {
        self.check_node_lease()?;
        let Some(base) = self.observed.value().ltx_root() else {
            self.appends_since_compaction_check = 0;
            return Ok(Some(false));
        };
        let replica = self.replica.clone();
        let scratch_directory = self.scratch_directory.clone();
        let attempt = replica.prepare_scheduled_compaction(&base, &scratch_directory);
        tokio::pin!(attempt);
        let prepared = loop {
            tokio::select! {
                result = &mut attempt => break result,
                _ = tokio::time::sleep_until(tokio::time::Instant::from_std(self.renew_at)) => {
                    self.renew().await?;
                }
            }
        };
        let prepared = match prepared {
            Ok(prepared) => prepared,
            Err(error) if retryable_ltx_error(&error) => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let Some(prepared) = prepared else {
            self.appends_since_compaction_check = 0;
            return Ok(Some(false));
        };
        let next_due_ms = self.observed.value().next_due_ms;
        self.publish_prepared(&prepared, next_due_ms).await?;
        self.segment_count = Some(prepared.verified().segment_count());
        self.appends_since_compaction_check = 0;
        Ok(Some(true))
    }

    /// Advances owner progress or fences when the renewal cannot be proven in time.
    pub(crate) async fn renew(&mut self) -> Result<()> {
        self.check_node_lease()?;
        let deadline = std::time::Instant::now() + SELF_FENCE_TIMEOUT;
        let deadline_at = tokio::time::Instant::from_std(deadline);
        let mut backoff = PublicationBackoff::default();
        loop {
            self.check_node_lease()?;
            let successor = self.observed.value().renew()?;
            let transition = tokio::time::timeout_at(
                deadline_at,
                self.authority
                    .transition(&self.observed, successor.clone(), Transition::Renew),
            )
            .await
            .map_err(|_| Error::Fenced)?;
            match transition {
                Ok(renewed) => {
                    self.check_node_lease()?;
                    if std::time::Instant::now() >= deadline {
                        return Err(Error::Fenced);
                    }
                    self.observed = renewed;
                    self.renew_at = std::time::Instant::now() + RENEW_INTERVAL;
                    return Ok(());
                }
                Err(error) => {
                    let current = tokio::time::timeout_at(
                        deadline_at,
                        self.authority.load(self.observed.value().cell),
                    )
                    .await
                    .map_err(|_| Error::Fenced)??
                    .ok_or(Error::Fenced)?;
                    if current.value().is_same_or_pure_renewal_of(&successor) {
                        self.observed = current;
                        self.renew_at = std::time::Instant::now() + RENEW_INTERVAL;
                        return Ok(());
                    }
                    let still_owned = current
                        .value()
                        .is_same_or_pure_renewal_of(self.observed.value());
                    if still_owned && retryable_publication_error(&error) {
                        self.observed = current;
                        backoff
                            .wait_until(runtime_retry_hint(&error), deadline)
                            .await?;
                        continue;
                    }
                    return Err(if still_owned { error } else { Error::Fenced });
                }
            }
        }
    }

    // Reconcile a lost activation CAS before exposing the restored handle.
    pub(crate) async fn activate(&mut self) -> Result<()> {
        self.check_node_lease()?;
        let mut backoff = PublicationBackoff::default();
        loop {
            self.check_node_lease()?;
            let successor = self.observed.value().activate()?;
            match self
                .authority
                .transition(&self.observed, successor.clone(), Transition::Activate)
                .await
            {
                Ok(activated) => {
                    self.check_node_lease()?;
                    self.observed = activated;
                    self.renew_at = std::time::Instant::now() + RENEW_INTERVAL;
                    return Ok(());
                }
                Err(error) => {
                    let current = loop {
                        match self.authority.load(self.observed.value().cell).await {
                            Ok(Some(current)) => break current,
                            Ok(None) => return Err(Error::Fenced),
                            Err(load_error) if retryable_publication_error(&load_error) => {
                                backoff.wait(runtime_retry_hint(&load_error)).await;
                            }
                            Err(load_error) => return Err(load_error),
                        }
                    };
                    if current.value().is_same_or_pure_renewal_of(&successor) {
                        self.observed = current;
                        self.renew_at = std::time::Instant::now() + RENEW_INTERVAL;
                        return Ok(());
                    }
                    let still_owned = current
                        .value()
                        .is_same_or_pure_renewal_of(self.observed.value());
                    if still_owned && retryable_publication_error(&error) {
                        self.observed = current;
                        backoff.wait(runtime_retry_hint(&error)).await;
                        continue;
                    }
                    return Err(if still_owned { error } else { Error::Fenced });
                }
            }
        }
    }

    pub(crate) async fn prepare(
        &mut self,
        pending: &crate::cell::executor::PendingCommit,
    ) -> Result<crab_ltx::PreparedRoot> {
        self.prepare_append(
            pending.cuts(),
            pending.outcome().commit_sequence(),
            self.observed.value().schema,
        )
        .await
    }

    pub(crate) async fn prepare_initial(
        &mut self,
        cuts: &crab_ltx::CaptureBatch,
    ) -> Result<crab_ltx::PreparedRoot> {
        if self.observed.value().root.is_some() {
            return Err(Error::Control("bootstrap control already has a root"));
        }
        let prepared = self
            .prepare_cuts(None, cuts, 0, self.observed.value().schema)
            .await?;
        self.note_append(&prepared);
        Ok(prepared)
    }

    /// Prepares the captured cut under its registry-selected target schema.
    pub async fn prepare_migration(
        &mut self,
        pending: &crate::cell::executor::PendingMigration,
    ) -> Result<crab_ltx::PreparedRoot> {
        if self.observed.value().schema != pending.from_schema() {
            return Err(Error::Fenced);
        }
        self.prepare_append(
            pending.cuts(),
            pending.commit_sequence(),
            pending.to_schema(),
        )
        .await
    }

    async fn prepare_append(
        &mut self,
        cuts: &crab_ltx::CaptureBatch,
        commit_sequence: u64,
        schema: u32,
    ) -> Result<crab_ltx::PreparedRoot> {
        let base = self.compact_before_append(cuts.segments.len()).await?;
        let prepared = match self
            .prepare_cuts(base.as_ref(), cuts, commit_sequence, schema)
            .await
        {
            Ok(prepared) => prepared,
            Err(Error::Ltx(error)) if error.is_cell_graph_limit() => {
                let Some(root) = base else {
                    return Err(error.into());
                };
                let Some(compacted) = self.force_full_compaction(&root).await? else {
                    return Err(error.into());
                };
                self.prepare_cuts(Some(&compacted), cuts, commit_sequence, schema)
                    .await?
            }
            Err(error) => return Err(error),
        };
        self.note_append(&prepared);
        Ok(prepared)
    }

    fn note_append(&mut self, prepared: &crab_ltx::PreparedRoot) {
        self.segment_count = Some(prepared.verified().segment_count());
        self.appends_since_compaction_check = self.appends_since_compaction_check.saturating_add(1);
        tracing::debug!(
            segments = prepared.verified().segment_count(),
            compaction_debt = self.appends_since_compaction_check,
            "Cell LTX append prepared"
        );
    }

    async fn compact_before_append(
        &mut self,
        incoming_segments: usize,
    ) -> Result<Option<crab_ltx::RootRef>> {
        let Some(mut base) = self.observed.value().ltx_root() else {
            return Ok(None);
        };
        let segment_count = match self.segment_count {
            Some(count) => count,
            None => {
                let count = self.replica.open_root(&base).await?.segment_count();
                self.segment_count = Some(count);
                count
            }
        };
        let segment_limit = self.replica.limits().max_segments.min(4_096);
        let projected = segment_count.saturating_add(incoming_segments);
        let debt_limit = COMPACTION_DEBT_SEGMENTS.min(segment_limit);
        let under_pressure = projected >= debt_limit;
        if !under_pressure {
            return Ok(Some(base));
        }
        tracing::debug!(
            segments = segment_count,
            incoming_segments,
            "Cell LTX compaction pressure"
        );

        for _ in 0..MAX_COMPACTION_CASCADE {
            let Some(prepared) = self.prepare_scheduled_compaction(&base).await? else {
                if self.segment_count.is_some_and(|count| {
                    count > 1 && count.saturating_add(incoming_segments) >= debt_limit
                }) && let Some(compacted) = self.force_full_compaction(&base).await?
                {
                    base = compacted;
                }
                self.appends_since_compaction_check = 0;
                return Ok(Some(base));
            };
            let next_due_ms = self.observed.value().next_due_ms;
            base = self.publish_prepared(&prepared, next_due_ms).await?;
            let compacted_segments = prepared.verified().segment_count();
            self.segment_count = Some(compacted_segments);
            if compacted_segments.saturating_add(incoming_segments) < debt_limit {
                self.appends_since_compaction_check = 0;
                return Ok(Some(base));
            }
        }
        Err(Error::Control(
            "Cell compaction cascade exceeded level limit",
        ))
    }

    async fn prepare_scheduled_compaction(
        &mut self,
        base: &crab_ltx::RootRef,
    ) -> Result<Option<crab_ltx::PreparedRoot>> {
        let mut backoff = PublicationBackoff::default();
        loop {
            let replica = self.replica.clone();
            let scratch_directory = self.scratch_directory.clone();
            let attempt = replica.prepare_scheduled_compaction(base, &scratch_directory);
            tokio::pin!(attempt);
            let result = loop {
                tokio::select! {
                    result = &mut attempt => break result,
                    _ = tokio::time::sleep_until(tokio::time::Instant::from_std(self.renew_at)) => {
                        self.renew().await?;
                    }
                }
            };
            match result {
                Ok(prepared) => return Ok(prepared),
                Err(error) if retryable_ltx_error(&error) => {
                    backoff.wait(ltx_retry_hint(&error)).await;
                }
                Err(error) => return Err(error.into()),
            }
        }
    }

    async fn force_full_compaction(
        &mut self,
        base: &crab_ltx::RootRef,
    ) -> Result<Option<crab_ltx::RootRef>> {
        let segment_count = self
            .segment_count
            .ok_or(Error::Control("Cell segment count is unavailable"))?;
        if segment_count <= 1 {
            return Ok(None);
        }
        tracing::debug!(segments = segment_count, "Cell LTX full compaction forced");
        let mut backoff = PublicationBackoff::default();
        let prepared = loop {
            let replica = self.replica.clone();
            let scratch_directory = self.scratch_directory.clone();
            let attempt = replica.prepare_compaction(base, 0..segment_count, 9, &scratch_directory);
            tokio::pin!(attempt);
            let result = loop {
                tokio::select! {
                    result = &mut attempt => break result,
                    _ = tokio::time::sleep_until(tokio::time::Instant::from_std(self.renew_at)) => {
                        self.renew().await?;
                    }
                }
            };
            match result {
                Ok(prepared) => break prepared,
                Err(error) if retryable_ltx_error(&error) => {
                    backoff.wait(ltx_retry_hint(&error)).await;
                }
                Err(error) => return Err(error.into()),
            }
        };
        let next_due_ms = self.observed.value().next_due_ms;
        let root = self.publish_prepared(&prepared, next_due_ms).await?;
        self.segment_count = Some(prepared.verified().segment_count());
        Ok(Some(root))
    }

    async fn prepare_cuts(
        &mut self,
        base: Option<&crab_ltx::RootRef>,
        cuts: &crab_ltx::CaptureBatch,
        commit_sequence: u64,
        schema: u32,
    ) -> Result<crab_ltx::PreparedRoot> {
        let mut backoff = PublicationBackoff::default();
        loop {
            let replica = self.replica.clone();
            let attempt = replica.prepare(base, cuts, commit_sequence, schema);
            tokio::pin!(attempt);
            let result = loop {
                tokio::select! {
                    result = &mut attempt => break result,
                    _ = tokio::time::sleep_until(tokio::time::Instant::from_std(self.renew_at)) => {
                        self.renew().await?;
                    }
                }
            };
            match result {
                Ok(prepared) => return Ok(prepared),
                Err(error) if retryable_ltx_error(&error) => {
                    backoff.wait(ltx_retry_hint(&error)).await;
                }
                Err(error) => return Err(error.into()),
            }
        }
    }

    pub(crate) async fn publish_prepared(
        &mut self,
        prepared: &crab_ltx::PreparedRoot,
        next_due_ms: Option<i64>,
    ) -> Result<crab_ltx::RootRef> {
        self.publish_proposal(prepared, next_due_ms, None).await
    }

    /// Publishes one root together with its new executable code/schema pair.
    pub async fn publish_migration(
        &mut self,
        prepared: &crab_ltx::PreparedRoot,
        next_due_ms: Option<i64>,
        code: crate::Digest,
        schema: u32,
    ) -> Result<crab_ltx::RootRef> {
        self.publish_proposal(prepared, next_due_ms, Some((code, schema)))
            .await
    }

    async fn publish_proposal(
        &mut self,
        prepared: &crab_ltx::PreparedRoot,
        next_due_ms: Option<i64>,
        migration: Option<(crate::Digest, u32)>,
    ) -> Result<crab_ltx::RootRef> {
        self.check_node_lease()?;
        let mut backoff = PublicationBackoff::default();
        loop {
            self.check_node_lease()?;
            let (successor, transition) = match migration {
                Some((code, schema)) => (
                    self.observed
                        .value()
                        .migrate_prepared(prepared, next_due_ms, code, schema)?,
                    Transition::Migrate,
                ),
                None => (
                    self.observed
                        .value()
                        .publish_prepared(prepared, next_due_ms)?,
                    Transition::Publish,
                ),
            };
            match self
                .authority
                .transition(&self.observed, successor.clone(), transition)
                .await
            {
                Ok(published) => {
                    self.check_node_lease()?;
                    self.observed = published;
                    self.renew_at = std::time::Instant::now() + RENEW_INTERVAL;
                    return Ok(prepared.root());
                }
                Err(error) => {
                    let current = loop {
                        match self.authority.load(self.observed.value().cell).await {
                            Ok(Some(current)) => break current,
                            Ok(None) => return Err(Error::Fenced),
                            Err(load_error) if retryable_publication_error(&load_error) => {
                                backoff.wait(runtime_retry_hint(&load_error)).await;
                            }
                            Err(load_error) => return Err(load_error),
                        }
                    };
                    if current.value().ltx_root() == Some(prepared.root()) {
                        self.observed = current;
                        return if self.observed.value().is_same_or_pure_renewal_of(&successor) {
                            self.renew_at = std::time::Instant::now() + RENEW_INTERVAL;
                            Ok(prepared.root())
                        } else {
                            Err(Error::Fenced)
                        };
                    }
                    if current
                        .value()
                        .is_same_or_pure_renewal_of(self.observed.value())
                        && retryable_publication_error(&error)
                    {
                        self.observed = current;
                        backoff.wait(runtime_retry_hint(&error)).await;
                        continue;
                    }
                    return Err(
                        if current
                            .value()
                            .is_same_or_pure_renewal_of(self.observed.value())
                        {
                            error
                        } else {
                            Error::Fenced
                        },
                    );
                }
            }
        }
    }

    /// Publishes the executor's retained commit or reconciles an ambiguous CAS.
    pub async fn publish_pending(&mut self, executor: &mut CellExecutor) -> Result<StoredOutcome> {
        let pending = executor.pending().ok_or(Error::PendingPublication)?;
        let next_due_ms = pending.next_due_ms();
        let prepared = self.prepare(pending).await?;
        executor.bind_prepared(&prepared)?;
        let root = match self.publish_prepared(&prepared, next_due_ms).await {
            Ok(root) => root,
            Err(error @ Error::Fenced) => {
                executor.fence();
                return Err(error);
            }
            Err(error) => return Err(error),
        };
        executor.confirm_published(&root)
    }

    /// Releases ownership after the SQL worker has closed the drained Cell.
    pub(crate) async fn release(&mut self) -> Result<()> {
        self.check_node_lease()?;
        let mut backoff = PublicationBackoff::default();
        loop {
            self.check_node_lease()?;
            let successor = self.observed.value().release()?;
            match self
                .authority
                .transition(&self.observed, successor.clone(), Transition::Release)
                .await
            {
                Ok(released) => {
                    self.check_node_lease()?;
                    self.observed = released;
                    return Ok(());
                }
                Err(error) => {
                    let current = loop {
                        match self.authority.load(self.observed.value().cell).await {
                            Ok(Some(current)) => break current,
                            Ok(None) => return Err(Error::Fenced),
                            Err(load_error) if retryable_publication_error(&load_error) => {
                                backoff.wait(runtime_retry_hint(&load_error)).await;
                            }
                            Err(load_error) => return Err(load_error),
                        }
                    };
                    if current.value() == &successor {
                        self.observed = current;
                        return Ok(());
                    }
                    let still_owned = current
                        .value()
                        .is_same_or_pure_renewal_of(self.observed.value());
                    if still_owned && retryable_publication_error(&error) {
                        self.observed = current;
                        backoff.wait(runtime_retry_hint(&error)).await;
                        continue;
                    }
                    return Err(if still_owned { error } else { Error::Fenced });
                }
            }
        }
    }

    /// Releases the newest control still owned by this fenced executor.
    ///
    /// Reloading first adopts an ambiguous publication or pure renewal. A new
    /// epoch or owner proves that recovery already belongs to another executor.
    pub(crate) async fn release_after_fence(&mut self) -> Result<()> {
        let expected = self.observed.value().clone();
        let expected_owner = expected.owner.clone().ok_or(Error::Fenced)?;
        let expected_cell = expected.cell;
        let expected_incarnation = expected.incarnation;
        let expected_epoch = expected.epoch;
        let mut backoff = PublicationBackoff::default();
        let current = loop {
            match self.authority.load(expected_cell).await {
                Ok(Some(current)) => break current,
                Ok(None) => return Err(Error::Fenced),
                Err(error) if retryable_publication_error(&error) => {
                    backoff.wait(runtime_retry_hint(&error)).await;
                }
                Err(error) => return Err(error),
            }
        };
        let value = current.value();
        if value.epoch != expected_epoch || value.owner.as_ref() != Some(&expected_owner) {
            return Ok(());
        }
        if value.incarnation != expected_incarnation
            || value.code != expected.code
            || value.schema != expected.schema
        {
            return Err(Error::Control("fenced control changed runtime identity"));
        }
        if value.root.is_none()
            || !matches!(
                value.state,
                crate::control::ControlState::Recovering | crate::control::ControlState::Serving
            )
        {
            return Err(Error::Control(
                "fenced active control lost its published root",
            ));
        }
        self.observed = current;
        self.release().await
    }

    fn check_node_lease(&self) -> Result<()> {
        self.node_lease
            .as_ref()
            .map_or(Ok(()), crate::NodeLeaseGuard::check)
    }
}

#[derive(Clone)]
pub(crate) struct PendingDurability {
    durability: std::sync::Arc<NodeDurability>,
    ticket: CommitTicket,
    submitted_at: std::time::Instant,
    telemetry: crate::fleet::telemetry::CellTelemetryHandle,
}

impl PendingDurability {
    pub(crate) async fn prove(&self) -> Result<()> {
        let proof = self.durability.prove(self.ticket).await?;
        if proof.source() == crate::node::log::DurabilitySource::Fleet {
            self.telemetry
                .durability_proof(proof.source(), self.submitted_at.elapsed());
        }
        Ok(())
    }

    pub(crate) async fn prove_fleet(&self) -> Result<()> {
        let proof = self.durability.prove_fleet(self.ticket).await?;
        self.telemetry
            .durability_proof(proof.source(), self.submitted_at.elapsed());
        Ok(())
    }

    pub(crate) async fn prove_object(&self) -> Result<()> {
        let proof = self.durability.prove_object(self.ticket).await?;
        self.telemetry
            .durability_proof(proof.source(), self.submitted_at.elapsed());
        Ok(())
    }
}

impl CellDurabilitySubmitter {
    pub(crate) async fn submit(
        &self,
        commit_sequence: u64,
        cuts: &crab_ltx::CaptureBatch,
    ) -> Result<Option<PendingDurability>> {
        let Some(slot) = self.node_durability.as_ref() else {
            self.telemetry
                .durability_submission(DurabilitySubmissionOutcome::Unsupported);
            return Ok(None);
        };
        let Some((application, durability)) = slot
            .read()
            .map_err(|_| Error::Control("Cell runtime node durability lock poisoned"))?
            .clone()
        else {
            self.telemetry
                .durability_submission(DurabilitySubmissionOutcome::Unavailable);
            return Ok(None);
        };
        self.check_node_lease()?;
        let submission = NodeLogSubmission::new(
            application,
            self.cell,
            self.incarnation,
            self.epoch,
            commit_sequence,
            cuts,
        )?;
        let ticket = match durability.submit(submission).await {
            Ok(ticket) => ticket,
            Err(error) => {
                self.check_node_lease()?;
                // The commit still succeeds through object coverage, so this
                // event and its counter are the only way to observe that an
                // enrolled lane refused the captured commit.
                tracing::warn!(
                    cell = %encode_hex(self.cell.as_bytes()),
                    error = %error,
                    "node-log submission rejected; using object coverage"
                );
                self.telemetry
                    .durability_submission(DurabilitySubmissionOutcome::Rejected);
                return Ok(None);
            }
        };
        self.telemetry
            .durability_submission(DurabilitySubmissionOutcome::Fleet);
        Ok(Some(PendingDurability {
            durability: std::sync::Arc::clone(&durability),
            ticket,
            submitted_at: std::time::Instant::now(),
            telemetry: self.telemetry.clone(),
        }))
    }

    fn check_node_lease(&self) -> Result<()> {
        self.node_lease
            .as_ref()
            .map_or(Ok(()), crate::NodeLeaseGuard::check)
    }
}

fn retryable_publication_error(error: &Error) -> bool {
    let Error::Storage(error) = error else {
        return false;
    };
    retryable_storage_error(error)
}

fn retryable_ltx_error(error: &crab_ltx::CrabError) -> bool {
    matches!(
        error,
        crab_ltx::CrabError::Storage(error)
            if retryable_storage_error(error)
    )
}

fn runtime_retry_hint(error: &Error) -> Option<std::time::Duration> {
    let Error::Storage(error) = error else {
        return None;
    };
    storage_retry_hint(error)
}

fn ltx_retry_hint(error: &crab_ltx::CrabError) -> Option<std::time::Duration> {
    let crab_ltx::CrabError::Storage(error) = error else {
        return None;
    };
    storage_retry_hint(error)
}

fn storage_retry_hint(error: &crab_storage::StorageError) -> Option<std::time::Duration> {
    match crab_storage::retry_class(error) {
        crab_storage::RetryClass::Throttled { retry_after } => retry_after,
        _ => None,
    }
}

fn retryable_storage_error(error: &crab_storage::StorageError) -> bool {
    matches!(
        crab_storage::retry_class(error),
        crab_storage::RetryClass::Transient
            | crab_storage::RetryClass::Throttled { .. }
            | crab_storage::RetryClass::StateDependent
            | crab_storage::RetryClass::InspectErrno
    )
}

struct PublicationBackoff {
    delay_ms: u64,
}

impl Default for PublicationBackoff {
    fn default() -> Self {
        Self { delay_ms: 100 }
    }
}

impl PublicationBackoff {
    async fn wait(&mut self, minimum: Option<std::time::Duration>) {
        let delay = std::time::Duration::from_millis(self.delay_ms);
        tokio::time::sleep(minimum.map_or(delay, |minimum| minimum.max(delay))).await;
        self.delay_ms = self.delay_ms.saturating_mul(2).min(MAX_RETRY_DELAY_MS);
    }

    async fn wait_until(
        &mut self,
        minimum: Option<std::time::Duration>,
        deadline: std::time::Instant,
    ) -> Result<()> {
        let delay = std::time::Duration::from_millis(self.delay_ms);
        let delay = minimum.map_or(delay, |minimum| minimum.max(delay));
        let remaining = deadline
            .checked_duration_since(std::time::Instant::now())
            .ok_or(Error::Fenced)?;
        if delay >= remaining {
            return Err(Error::Fenced);
        }
        tokio::time::sleep(delay).await;
        self.delay_ms = self.delay_ms.saturating_mul(2).min(MAX_RETRY_DELAY_MS);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use bytes::Bytes;
    use crab_ltx::{CaptureBatch, CellReplica, CellStorageLayout, Db, Limits};
    use crab_storage::Store;
    use object_store::{memory::InMemory, path::Path};

    use super::{
        CellDurabilitySubmitter, CellPublisher, DurabilitySubmissionOutcome, NodeDurabilitySlot,
    };
    use crate::Error;
    use crate::control::authority::CellAuthority;
    use crate::control::{Control, Owner};
    use crate::identity::IncarnationId;
    use crate::identity::{CellId, Digest, SessionId};

    #[tokio::test]
    async fn quiet_compaction_publishes_exact_root_after_eight_appends() {
        let directory = tempfile::tempdir().unwrap();
        let mut database =
            Db::open(&directory.path().join("cell.sqlite"), Limits::default()).unwrap();
        let cell = CellId::from_bytes([41; 32]);
        let incarnation = IncarnationId::from_bytes([42; 16]);
        let layout = CellStorageLayout::new(
            Store::new(Arc::new(InMemory::new())),
            Path::from("quiet-compaction"),
            [43; 16],
        );
        let replica = CellReplica::new(
            layout.clone(),
            *cell.as_bytes(),
            *incarnation.as_bytes(),
            Limits::default(),
        )
        .unwrap();
        let control = Control::initial(
            cell,
            incarnation,
            Owner {
                session: SessionId::from_bytes([44; 16]),
                endpoint: "https://node.internal:8081".into(),
            },
            Digest::from_bytes([45; 32]),
            1,
        )
        .unwrap();
        layout
            .store()
            .create_strict(
                &layout.control_path(cell.as_bytes()),
                Bytes::from(control.encode().unwrap()),
            )
            .await
            .unwrap();
        let authority = CellAuthority::new(layout);
        let observed = authority.load(cell).await.unwrap().unwrap();
        let mut publisher = CellPublisher::new(
            replica.clone(),
            authority,
            observed,
            directory.path().to_owned(),
        );
        for sequence in 1..=8_u64 {
            database
                .transaction(|transaction| {
                    if sequence == 1 {
                        transaction
                            .execute_batch("CREATE TABLE events(sequence INTEGER PRIMARY KEY)")?;
                    }
                    transaction.execute("INSERT INTO events VALUES (?1)", [sequence])?;
                    Ok(())
                })
                .unwrap();
            let cuts = database.capture_deferred().unwrap();
            let prepared = publisher.prepare_append(&cuts, sequence, 1).await.unwrap();
            publisher.publish_prepared(&prepared, None).await.unwrap();
        }
        assert!(publisher.compaction_due());
        let before = publisher.control().value().ltx_root().unwrap();
        assert_eq!(replica.open_root(&before).await.unwrap().segment_count(), 8);
        assert_eq!(publisher.compact_one_quiet().await.unwrap(), Some(true));
        let after = publisher.control().value().ltx_root().unwrap();
        assert_eq!(after.position, before.position);
        assert_eq!(after.commit_sequence, before.commit_sequence);
        assert_eq!(replica.open_root(&after).await.unwrap().segment_count(), 1);
        assert_eq!(publisher.compact_one_quiet().await.unwrap(), Some(false));
        assert!(!publisher.compaction_due());

        let mut segments = Vec::new();
        let mut position = after.position;
        for sequence in 9..=10_u64 {
            database
                .transaction(|transaction| {
                    transaction.execute("INSERT INTO events VALUES (?1)", [sequence])?;
                    Ok(())
                })
                .unwrap();
            let captured = database.capture_deferred().unwrap();
            segments.extend(captured.segments);
            position = captured.position;
        }
        let cuts = CaptureBatch {
            segments,
            position,
            timing: Default::default(),
        };
        assert_eq!(cuts.segments.len(), 2);
        let prepared = publisher.prepare_append(&cuts, 9, 1).await.unwrap();
        publisher.publish_prepared(&prepared, None).await.unwrap();
        let extended = publisher.control().value().ltx_root().unwrap();
        assert_eq!(extended.position, position);
        assert_eq!(
            replica.open_root(&extended).await.unwrap().segment_count(),
            3
        );

        for sequence in 10..=37_u64 {
            database
                .transaction(|transaction| {
                    transaction.execute("INSERT INTO events VALUES (?1)", [sequence + 1])?;
                    Ok(())
                })
                .unwrap();
            let cuts = database.capture_deferred().unwrap();
            let prepared = publisher.prepare_append(&cuts, sequence, 1).await.unwrap();
            publisher.publish_prepared(&prepared, None).await.unwrap();
        }
        let at_ceiling = publisher.control().value().ltx_root().unwrap();
        assert_eq!(
            replica
                .open_root(&at_ceiling)
                .await
                .unwrap()
                .segment_count(),
            31
        );
        database
            .transaction(|transaction| {
                transaction.execute("INSERT INTO events VALUES (39)", [])?;
                Ok(())
            })
            .unwrap();
        let cuts = database.capture_deferred().unwrap();
        let prepared = publisher.prepare_append(&cuts, 38, 1).await.unwrap();
        publisher.publish_prepared(&prepared, None).await.unwrap();
        let forced = publisher.control().value().ltx_root().unwrap();
        assert!(replica.open_root(&forced).await.unwrap().segment_count() < 32);
        database.close().unwrap();
    }

    #[derive(Default)]
    struct RecordingSubmissions {
        outcomes: std::sync::Mutex<Vec<DurabilitySubmissionOutcome>>,
    }

    impl crate::fleet::telemetry::CellTelemetry for RecordingSubmissions {
        fn durability_submission(&self, outcome: DurabilitySubmissionOutcome) {
            self.outcomes.lock().unwrap().push(outcome);
        }
    }

    #[tokio::test]
    async fn commits_report_when_no_enrolled_lane_can_carry_them() {
        let telemetry = crate::fleet::telemetry::CellTelemetryHandle::default();
        let recording = Arc::new(RecordingSubmissions::default());
        telemetry.install(recording.clone()).unwrap();
        let cuts = CaptureBatch {
            segments: Vec::new(),
            position: Default::default(),
            timing: Default::default(),
        };
        let submitter = CellDurabilitySubmitter {
            cell: CellId::from_bytes([71; 32]),
            incarnation: IncarnationId::from_bytes([72; 16]),
            epoch: 1,
            node_lease: None,
            node_durability: None,
            telemetry: telemetry.clone(),
        };
        assert!(submitter.submit(1, &cuts).await.unwrap().is_none());

        let lane: NodeDurabilitySlot = Arc::new(std::sync::RwLock::new(None));
        let submitter = CellDurabilitySubmitter {
            node_durability: Some(lane),
            telemetry,
            ..submitter
        };
        assert!(submitter.submit(1, &cuts).await.unwrap().is_none());

        assert_eq!(
            *recording.outcomes.lock().unwrap(),
            vec![
                DurabilitySubmissionOutcome::Unsupported,
                DurabilitySubmissionOutcome::Unavailable,
            ]
        );
    }

    /// Transport that refuses every follower request; the gate is fenced first,
    /// so no frame reaches it in this test.
    struct RefusingTransport;

    impl crate::node::log_transport::NodeLogTransport for RefusingTransport {
        fn append<'a>(
            &'a self,
            _member: crate::identity::NodeId,
            _request: crate::node::log_transport::AppendRequest,
        ) -> futures_util::future::BoxFuture<'a, crate::Result<crate::follower::FollowerReceipt>>
        {
            Box::pin(async { Err(Error::Node("test transport refuses appends")) })
        }

        fn seal<'a>(
            &'a self,
            _member: crate::identity::NodeId,
            _request: crate::node::log_transport::SealRequest,
        ) -> futures_util::future::BoxFuture<'a, crate::Result<crate::follower::FollowerReceipt>>
        {
            Box::pin(async { Err(Error::Node("test transport refuses seals")) })
        }

        fn retire<'a>(
            &'a self,
            _member: crate::identity::NodeId,
            _request: crate::node::log_transport::RetireRequest,
        ) -> futures_util::future::BoxFuture<'a, crate::Result<crate::follower::FollowerReceipt>>
        {
            Box::pin(async { Err(Error::Node("test transport refuses retirements")) })
        }

        fn tail<'a>(
            &'a self,
            _member: crate::identity::NodeId,
            _request: crate::node::log_transport::TailRequest,
        ) -> futures_util::future::BoxFuture<'a, crate::Result<Vec<bytes::Bytes>>> {
            Box::pin(async { Err(Error::Node("test transport refuses tails")) })
        }
    }

    /// Authority that refuses activation; the fenced gate never asks it anything.
    struct RefusingAuthority;

    impl crate::node::durability::NodeLogAuthority for RefusingAuthority {
        fn activate<'a>(
            &'a self,
            _log_epoch: u64,
        ) -> futures_util::future::BoxFuture<'a, crate::Result<()>> {
            Box::pin(async { Err(Error::Node("test authority refuses activation")) })
        }

        fn advance_coverage<'a>(
            &'a self,
            _log_epoch: u64,
            _tiered_through: u64,
        ) -> futures_util::future::BoxFuture<'a, crate::Result<()>> {
            Box::pin(async { Err(Error::Node("test authority refuses coverage")) })
        }

        fn close<'a>(
            &'a self,
            _barrier: &'a crate::node::log::NodeLogRotationBarrier,
        ) -> futures_util::future::BoxFuture<'a, crate::Result<()>> {
            Box::pin(async { Err(Error::Node("test authority refuses closing")) })
        }
    }

    #[tokio::test]
    async fn commits_report_a_fenced_lane_instead_of_failing() {
        let telemetry = crate::fleet::telemetry::CellTelemetryHandle::default();
        let recording = Arc::new(RecordingSubmissions::default());
        telemetry.install(recording.clone()).unwrap();

        let gate = crate::node::log::DurabilityGate::new(
            SessionId::from_bytes([81; 16]),
            crate::identity::NodeId::from_bytes([82; 16]),
            9,
            [crate::identity::NodeId::from_bytes([83; 16])],
        )
        .unwrap();
        let transport: Arc<dyn crate::node::log_transport::NodeLogTransport> =
            Arc::new(RefusingTransport);
        let shipper = crate::node::log_shipper::NodeLogShipper::new_with_telemetry(
            gate.clone(),
            Arc::clone(&transport),
            Limits::default(),
            telemetry.clone(),
        )
        .unwrap();
        let lease = crate::node::lease::NodeLeaseGuard::new(0, 60_000).unwrap();
        let durability = Arc::new(crate::node::durability::NodeDurability::new(
            gate.clone(),
            shipper,
            Arc::new(RefusingAuthority),
            transport,
            lease,
        ));
        gate.stop_shipping();

        let directory = tempfile::tempdir().unwrap();
        let mut database =
            Db::open(&directory.path().join("cell.sqlite"), Limits::default()).unwrap();
        database
            .transaction(|transaction| {
                transaction.execute_batch("CREATE TABLE events(sequence INTEGER PRIMARY KEY)")?;
                transaction.execute("INSERT INTO events VALUES (1)", [])?;
                Ok(())
            })
            .unwrap();
        let cuts = database.capture_deferred().unwrap();

        let submitter = CellDurabilitySubmitter {
            cell: CellId::from_bytes([84; 32]),
            incarnation: IncarnationId::from_bytes([85; 16]),
            epoch: 1,
            node_lease: None,
            node_durability: Some(Arc::new(std::sync::RwLock::new(Some((
                crate::identity::ApplicationId::from_bytes([86; 16]),
                durability,
            ))))),
            telemetry,
        };
        assert!(submitter.submit(1, &cuts).await.unwrap().is_none());
        assert_eq!(
            *recording.outcomes.lock().unwrap(),
            vec![DurabilitySubmissionOutcome::Rejected]
        );
        database.close().unwrap();
    }
}
