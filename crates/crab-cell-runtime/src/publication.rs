use crate::{
    ApplicationId, CellAuthority, CellExecutor, CommitTicket, Error, NodeDurability,
    NodeLogSubmission, Result, StoredOutcome, Transition, VersionedControl,
};

const MAX_RETRY_DELAY_MS: u64 = 1_000;
const COMPACTION_CHECK_INTERVAL: u8 = 8;
const MAX_COMPACTION_CASCADE: usize = 9;
const RENEW_INTERVAL: std::time::Duration = std::time::Duration::from_secs(3);
const SELF_FENCE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

pub(crate) type NodeDurabilityBinding = (ApplicationId, std::sync::Arc<NodeDurability>);

#[derive(Clone)]
pub(crate) struct CellDurabilitySubmitter {
    cell: crate::CellId,
    incarnation: crate::IncarnationId,
    epoch: u64,
    node_lease: Option<crate::NodeLeaseGuard>,
    node_durability: Option<std::sync::Arc<std::sync::OnceLock<NodeDurabilityBinding>>>,
    telemetry: crate::CellTelemetryHandle,
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
    node_durability: Option<std::sync::Arc<std::sync::OnceLock<NodeDurabilityBinding>>>,
    telemetry: crate::CellTelemetryHandle,
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
            appends_since_compaction_check: COMPACTION_CHECK_INTERVAL,
            renew_at: std::time::Instant::now() + RENEW_INTERVAL,
            node_lease: None,
            node_durability: None,
            telemetry: crate::CellTelemetryHandle::default(),
        }
    }

    pub(crate) fn with_node_lease(mut self, node_lease: crate::NodeLeaseGuard) -> Self {
        self.node_lease = Some(node_lease);
        self
    }

    pub(crate) fn with_node_durability_slot(
        mut self,
        durability: std::sync::Arc<std::sync::OnceLock<NodeDurabilityBinding>>,
    ) -> Self {
        self.node_durability = Some(durability);
        self
    }

    pub(crate) fn with_telemetry(mut self, telemetry: crate::CellTelemetryHandle) -> Self {
        self.telemetry = telemetry;
        self
    }

    pub(crate) async fn submit_migration_durability(
        &self,
        pending: &crate::PendingMigration,
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
            .durability_proof(crate::DurabilitySource::Object, waited);
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
        pending: &crate::PendingCommit,
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
        pending: &crate::PendingMigration,
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
        let base = self.compact_before_append().await?;
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
    }

    async fn compact_before_append(&mut self) -> Result<Option<crab_ltx::RootRef>> {
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
        let under_pressure = segment_count >= segment_limit.saturating_sub(1).max(1);
        if !under_pressure && self.appends_since_compaction_check < COMPACTION_CHECK_INTERVAL {
            return Ok(Some(base));
        }

        for _ in 0..MAX_COMPACTION_CASCADE {
            let Some(prepared) = self.prepare_scheduled_compaction(&base).await? else {
                self.appends_since_compaction_check = 0;
                return Ok(Some(base));
            };
            let next_due_ms = self.observed.value().next_due_ms;
            base = self.publish_prepared(&prepared, next_due_ms).await?;
            self.segment_count = Some(prepared.verified().segment_count());
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
                crate::ControlState::Recovering | crate::ControlState::Serving
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
    telemetry: crate::CellTelemetryHandle,
}

impl PendingDurability {
    pub(crate) async fn prove(&self) -> Result<()> {
        let proof = self.durability.prove(self.ticket).await?;
        if proof.source() == crate::DurabilitySource::Fleet {
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
        let Some((application, durability)) = self
            .node_durability
            .as_ref()
            .and_then(|durability| durability.get())
        else {
            return Ok(None);
        };
        self.check_node_lease()?;
        let submission = NodeLogSubmission::new(
            *application,
            self.cell,
            self.incarnation,
            self.epoch,
            commit_sequence,
            cuts,
        )?;
        let ticket = match durability.submit(submission).await {
            Ok(ticket) => ticket,
            Err(_) => {
                self.check_node_lease()?;
                return Ok(None);
            }
        };
        Ok(Some(PendingDurability {
            durability: std::sync::Arc::clone(durability),
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
