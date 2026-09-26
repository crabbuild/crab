//! Resolving, acquiring, activating, and taking over a local Cell.
//!
//! Every entry point here turns a catalog proof, a restored root, or a
//! takeover proof into a running local handle, and refuses when the
//! authority, capacity ledger, or node lease does not agree.

use super::*;

impl CellRuntime {
    /// Resolves an active local owner without exposing the dispatcher's Cell map.
    pub async fn local_handle(
        &self,
        catalog: CatalogProof,
        control: &VersionedControl,
    ) -> crate::Result<Option<CellHandle>> {
        self.ensure_running()?;
        let value = control.value();
        if catalog.entry().cell() != value.cell {
            return Err(Error::Control("scheduler catalog and control differ"));
        }
        if value
            .owner
            .as_ref()
            .is_none_or(|owner| owner.session != self.inner.session)
            || value.root.is_none()
        {
            return Ok(None);
        }
        let (reply, response) = oneshot::channel();
        self.inner
            .sender
            .send(Message::Lookup {
                cell: value.cell,
                require_resident: false,
                reply,
            })
            .await
            .map_err(|_| Error::RuntimeClosed)?;
        let Some(local) = response.await.map_err(|_| Error::RuntimeClosed)? else {
            return Ok(None);
        };
        if local.incarnation != value.incarnation
            || local.code != value.code
            || local.schema != value.schema
        {
            return Ok(None);
        }
        Ok(Some(CellHandle {
            cell: value.cell,
            incarnation: value.incarnation,
            code: value.code,
            schema: value.schema,
            catalog,
            inner: self.inner.clone(),
            admission: local.admission,
        }))
    }

    /// Creates, initializes and publishes a new Cell before returning a handle.
    pub async fn bootstrap<F>(
        &self,
        catalog: CatalogProof,
        replica: crab_ltx::CellReplica,
        authority: CellAuthority,
        observed: VersionedControl,
        destination: PathBuf,
        initialize: F,
    ) -> crate::Result<CellHandle>
    where
        F: for<'connection> FnOnce(
                &crab_ltx::rusqlite::Transaction<'connection>,
            ) -> crate::Result<()>
            + Send
            + 'static,
    {
        self.ensure_acquiring()?;
        self.check_application_limits(&catalog, replica.limits())?;
        self.activation_cell(&catalog, &observed)?;
        if observed.value().state != crate::control::ControlState::Recovering
            || observed.value().root.is_some()
        {
            return Err(Error::Control("bootstrap requires an unpublished control"));
        }
        let reservation = self.inner.pool.reserve_activation()?;
        let incarnation = observed.value().incarnation;
        let schema = observed.value().schema;
        self.activate_inner(
            catalog,
            Activation::Bootstrap(Box::new(BootstrapActivation {
                replica: replica.clone(),
                destination,
                incarnation,
                schema,
                initialize: Box::new(initialize),
                reservation,
            })),
            replica,
            authority,
            observed,
        )
        .await
    }

    /// Takes over an unchanged unpublished owner, initializes and publishes the Cell.
    #[expect(
        clippy::too_many_arguments,
        reason = "the takeover boundary keeps every authority and activation input explicit"
    )]
    pub async fn takeover_unpublished<F>(
        &self,
        catalog: CatalogProof,
        replica: crab_ltx::CellReplica,
        authority: CellAuthority,
        mut observed: VersionedControl,
        takeover: crate::node::NodeTakeoverProof,
        destination: PathBuf,
        owner: Owner,
        initialize: F,
    ) -> crate::Result<CellHandle>
    where
        F: for<'connection> FnOnce(
                &crab_ltx::rusqlite::Transaction<'connection>,
            ) -> crate::Result<()>
            + Send
            + 'static,
    {
        self.ensure_acquiring()?;
        self.check_application_limits(&catalog, replica.limits())?;
        let cell = self.claiming_cell(&catalog, &observed, &owner)?;
        if owner.session != takeover.claimant() {
            return Err(Error::Fenced);
        }
        loop {
            if observed.value().state != crate::control::ControlState::Recovering
                || observed.value().owner.is_none()
                || observed.value().root.is_some()
            {
                return Err(Error::Control(
                    "unpublished takeover requires an active rootless control",
                ));
            }
            if observed.value().owner.as_ref().map(|owner| owner.session)
                != Some(takeover.session())
            {
                return Err(Error::Fenced);
            }
            self.ensure_running()?;
            let current = authority.load(cell).await?.ok_or(Error::Fenced)?;
            if current.value() != observed.value() {
                self.claiming_cell(&catalog, &current, &owner)?;
                observed = current;
                continue;
            }
            let reservation = self.inner.pool.reserve_activation()?;
            let successor = current.value().takeover(owner.clone())?;
            let claimed = match authority
                .transition(&current, successor.clone(), Transition::Takeover)
                .await
            {
                Ok(claimed) => claimed,
                Err(error) => {
                    let latest = authority.load(cell).await?.ok_or(Error::Fenced)?;
                    if latest.value() == &successor {
                        latest
                    } else if matches!(
                        &error,
                        Error::Storage(crab_storage::StorageError::StateConflict { .. })
                    ) {
                        observed = latest;
                        continue;
                    } else {
                        return Err(error);
                    }
                }
            };
            let incarnation = claimed.value().incarnation;
            let schema = claimed.value().schema;
            return self
                .activate_inner(
                    catalog,
                    Activation::Bootstrap(Box::new(BootstrapActivation {
                        replica: replica.clone(),
                        destination,
                        incarnation,
                        schema,
                        initialize: Box::new(initialize),
                        reservation,
                    })),
                    replica,
                    authority,
                    claimed,
                )
                .await;
        }
    }

    /// Cold-opens the exact authoritative root on the Cell's SQL worker.
    pub async fn activate_restored(
        &self,
        catalog: CatalogProof,
        replica: crab_ltx::CellReplica,
        authority: CellAuthority,
        observed: VersionedControl,
        recovery_store: crate::recovery::manifest::RecoveryManifestStore,
        destination: PathBuf,
    ) -> crate::Result<CellHandle> {
        self.ensure_acquiring()?;
        self.check_application_limits(&catalog, replica.limits())?;
        self.check_application_limits(&catalog, recovery_store.limits())?;
        self.activation_cell(&catalog, &observed)?;
        let replica = self.replica_with_directory_cache(replica, &destination)?;
        let observed = self
            .publish_attached_recovery(&replica, &authority, observed, &recovery_store)
            .await?;
        let reservation = self.inner.pool.reserve_activation()?;
        self.activate_restored_reserved(
            catalog,
            replica,
            authority,
            observed,
            destination,
            reservation,
        )
        .await
    }

    /// Acquires an idle published Cell and restores its exact immutable root.
    pub async fn acquire_idle_restored(
        &self,
        catalog: CatalogProof,
        replica: crab_ltx::CellReplica,
        authority: CellAuthority,
        observed: VersionedControl,
        destination: PathBuf,
        owner: Owner,
    ) -> crate::Result<CellHandle> {
        self.ensure_acquiring()?;
        self.check_application_limits(&catalog, replica.limits())?;
        let rollback_node_lease = self.inner.node_lease.guard()?;
        self.claiming_cell(&catalog, &observed, &owner)?;
        if observed.value().state != crate::control::ControlState::Idle
            || observed.value().owner.is_some()
            || observed.value().root.is_none()
        {
            return Err(Error::Control(
                "idle acquisition requires a published idle control",
            ));
        }
        let reservation = self.inner.pool.reserve_activation()?;
        let successor = observed.value().takeover(owner)?;
        let ownership_started = std::time::Instant::now();
        let claimed = match authority
            .transition(&observed, successor.clone(), Transition::Takeover)
            .await
        {
            Ok(claimed) => claimed,
            Err(error) => {
                let current = authority
                    .load(observed.value().cell)
                    .await?
                    .ok_or(Error::Fenced)?;
                if current.value() != &successor {
                    return Err(error);
                }
                current
            }
        };
        self.inner.telemetry.activation_phase(
            crate::fleet::telemetry::ActivationPhase::Ownership,
            ownership_started.elapsed(),
        );
        let rollback_authority = authority.clone();
        let rollback_claim = claimed.clone();
        let rollback_replica = replica.clone();
        match self
            .activate_restored_reserved(
                catalog,
                replica,
                authority,
                claimed,
                destination,
                reservation,
            )
            .await
        {
            Ok(handle) => Ok(handle),
            Err(error) => {
                match rollback_failed_acquisition(
                    &rollback_authority,
                    &rollback_claim,
                    &rollback_replica,
                    rollback_node_lease,
                )
                .await
                {
                    Ok(()) => Err(error),
                    Err(cleanup) => Err(cleanup),
                }
            }
        }
    }

    /// Takes over an unchanged owner after its exact node session is fenced.
    #[expect(
        clippy::too_many_arguments,
        reason = "the takeover boundary keeps every authority, recovery and activation input explicit"
    )]
    pub async fn takeover_restored(
        &self,
        catalog: CatalogProof,
        replica: crab_ltx::CellReplica,
        authority: CellAuthority,
        mut observed: VersionedControl,
        takeover: crate::node::NodeTakeoverProof,
        recovery_store: crate::recovery::manifest::RecoveryManifestStore,
        destination: PathBuf,
        owner: Owner,
    ) -> crate::Result<CellHandle> {
        self.ensure_acquiring()?;
        self.check_application_limits(&catalog, replica.limits())?;
        self.check_application_limits(&catalog, recovery_store.limits())?;
        let replica = self.replica_with_directory_cache(replica, &destination)?;
        let rollback_node_lease = self.inner.node_lease.guard()?;
        let rollback_authority = authority.clone();
        let rollback_replica = replica.clone();
        let cell = self.claiming_cell(&catalog, &observed, &owner)?;
        if owner.session != takeover.claimant() {
            return Err(Error::Fenced);
        }
        loop {
            if !matches!(
                observed.value().state,
                crate::control::ControlState::Recovering | crate::control::ControlState::Serving
            ) || observed.value().owner.is_none()
                || observed.value().root.is_none()
            {
                return Err(Error::Control(
                    "takeover requires a published control with an active owner",
                ));
            }
            if observed.value().owner.as_ref().map(|owner| owner.session)
                != Some(takeover.session())
            {
                return Err(Error::Fenced);
            }
            self.ensure_running()?;
            let current = authority.load(cell).await?.ok_or(Error::Fenced)?;
            if current.value() != observed.value() {
                self.claiming_cell(&catalog, &current, &owner)?;
                observed = current;
                continue;
            }
            let reservation = self.inner.pool.reserve_activation()?;
            let successor = current.value().takeover(owner.clone())?;
            let claimed = match authority
                .transition(&current, successor.clone(), Transition::Takeover)
                .await
            {
                Ok(claimed) => claimed,
                Err(error) => {
                    let latest = authority.load(cell).await?.ok_or(Error::Fenced)?;
                    if latest.value() == &successor {
                        latest
                    } else if matches!(
                        &error,
                        Error::Storage(crab_storage::StorageError::StateConflict { .. })
                    ) {
                        observed = latest;
                        continue;
                    } else {
                        return Err(error);
                    }
                }
            };
            let recovery_rollback_claim = claimed.clone();
            let claimed = match self
                .publish_attached_recovery(&replica, &authority, claimed, &recovery_store)
                .await
            {
                Ok(claimed) => claimed,
                Err(error) => {
                    match rollback_failed_acquisition(
                        &rollback_authority,
                        &recovery_rollback_claim,
                        &rollback_replica,
                        rollback_node_lease.clone(),
                    )
                    .await
                    {
                        Ok(()) => return Err(error),
                        Err(cleanup) => return Err(cleanup),
                    }
                }
            };
            let rollback_claim = claimed.clone();
            return match self
                .activate_restored_reserved(
                    catalog,
                    replica,
                    authority,
                    claimed,
                    destination,
                    reservation,
                )
                .await
            {
                Ok(handle) => Ok(handle),
                Err(error) => {
                    match rollback_failed_acquisition(
                        &rollback_authority,
                        &rollback_claim,
                        &rollback_replica,
                        rollback_node_lease,
                    )
                    .await
                    {
                        Ok(()) => Err(error),
                        Err(cleanup) => Err(cleanup),
                    }
                }
            };
        }
    }

    async fn publish_attached_recovery(
        &self,
        replica: &crab_ltx::CellReplica,
        authority: &CellAuthority,
        observed: VersionedControl,
        recovery_store: &crate::recovery::manifest::RecoveryManifestStore,
    ) -> crate::Result<VersionedControl> {
        let Some(recovery) = observed.value().recovery.as_ref() else {
            return Ok(observed);
        };
        let overlay = recovery_store
            .load_overlay(
                observed.value().cell,
                observed.value().incarnation,
                recovery,
            )
            .await?;
        let prepared = replica
            .prepare_recovered_overlay(&overlay, observed.value().schema)
            .await?;
        self.ensure_running()?;
        let successor = observed
            .value()
            .publish_recovery(&prepared, observed.value().next_due_ms)?;
        match authority
            .transition(&observed, successor.clone(), Transition::PublishRecovery)
            .await
        {
            Ok(published) => Ok(published),
            Err(error) => {
                let current = authority
                    .load(observed.value().cell)
                    .await?
                    .ok_or(Error::Fenced)?;
                if current.value() == &successor {
                    Ok(current)
                } else {
                    Err(error)
                }
            }
        }
    }

    async fn activate_restored_reserved(
        &self,
        catalog: CatalogProof,
        replica: crab_ltx::CellReplica,
        authority: CellAuthority,
        observed: VersionedControl,
        destination: PathBuf,
        reservation: CellReservation,
    ) -> crate::Result<CellHandle> {
        // Root verification precedes actor activation, so it must use the same
        // persistent directory cache as the publisher path below.
        let replica = self.replica_with_directory_cache(replica, &destination)?;
        let cell = self.activation_cell(&catalog, &observed)?;
        let control = observed.value();
        let root = control
            .ltx_root()
            .ok_or(Error::Control("activation requires a published root"))?;
        let incarnation = control.incarnation;
        let schema = control.schema;
        // A resume record this node wrote on a clean release still names this
        // exact root, so the local image can be continued instead of restored.
        // The record is an accelerator: every failure falls back to the origin.
        let database = match crate::cell::resume::take_matching(&destination, control, &replica) {
            Some(source) => {
                let resumed_started = std::time::Instant::now();
                match replica.open_resumed(&source, &destination) {
                    Ok(db) => {
                        self.inner.telemetry.activation_phase(
                            crate::fleet::telemetry::ActivationPhase::Resume,
                            resumed_started.elapsed(),
                        );
                        crate::cell::worker::RestoredDatabase::Local(Box::new(db))
                    }
                    Err(error) => {
                        tracing::debug!(error = %error, "Cell resume record did not continue");
                        let _ = replica.discard_resumed(&destination);
                        let _ = replica.discard_resumed(&source);
                        self.restore_exact(&replica, &root, schema, &destination)
                            .await?
                    }
                }
            }
            None => {
                self.restore_exact(&replica, &root, schema, &destination)
                    .await?
            }
        };
        let current = authority.load(cell).await?.ok_or(Error::Fenced)?;
        if !current.value().is_same_or_pure_renewal_of(observed.value()) {
            return Err(Error::Fenced);
        }
        let activate_started = std::time::Instant::now();
        let handle = self
            .activate_inner(
                catalog,
                Activation::Restored(Box::new(RestoredActivation {
                    database,
                    destination,
                    incarnation,
                    schema,
                    root,
                    reservation,
                })),
                replica,
                authority,
                current,
            )
            .await?;
        self.inner.telemetry.activation_phase(
            crate::fleet::telemetry::ActivationPhase::Activate,
            activate_started.elapsed(),
        );
        Ok(handle)
    }

    /// Verifies the immutable root and materializes it into the destination.
    async fn restore_exact(
        &self,
        replica: &crab_ltx::CellReplica,
        root: &crab_ltx::RootRef,
        schema: u32,
        destination: &Path,
    ) -> crate::Result<crate::cell::worker::RestoredDatabase> {
        let root_open_started = std::time::Instant::now();
        let verified = replica.open_root(root).await?;
        self.inner.telemetry.activation_phase(
            crate::fleet::telemetry::ActivationPhase::RootOpen,
            root_open_started.elapsed(),
        );
        if verified.schema() != schema {
            return Err(Error::Control(
                "immutable root schema does not match control",
            ));
        }
        let restore_started = std::time::Instant::now();
        let database = verified.paged().prepare_writable(destination).await?;
        self.inner.telemetry.activation_phase(
            crate::fleet::telemetry::ActivationPhase::Restore,
            restore_started.elapsed(),
        );
        Ok(crate::cell::worker::RestoredDatabase::Paged(Box::new(
            database,
        )))
    }

    fn claiming_cell(
        &self,
        catalog: &CatalogProof,
        observed: &VersionedControl,
        owner: &Owner,
    ) -> crate::Result<CellId> {
        let cell = catalog.entry().cell();
        if observed.value().cell != cell {
            return Err(Error::Control("ownership control changed Cell"));
        }
        if owner.session != self.inner.session {
            return Err(Error::Fenced);
        }
        if observed
            .value()
            .owner
            .as_ref()
            .is_some_and(|current| current.session == owner.session)
        {
            return Err(Error::CellAlreadyActive);
        }
        Ok(cell)
    }

    fn check_application_limits(
        &self,
        catalog: &CatalogProof,
        limits: crab_ltx::Limits,
    ) -> crate::Result<()> {
        let Some(application_limits) = self.inner.application_limits.get() else {
            return Ok(());
        };
        let Some(&(database, capture)) = application_limits.get(&catalog.entry().namespace())
        else {
            return Err(Error::Control(
                "Cell namespace is not declared by application",
            ));
        };
        if limits.max_database_bytes != database || limits.max_capture_bytes != capture {
            return Err(Error::Control(
                "Cell storage limits differ from application",
            ));
        }
        Ok(())
    }

    fn activation_cell(
        &self,
        catalog: &CatalogProof,
        observed: &VersionedControl,
    ) -> crate::Result<CellId> {
        let cell = catalog.entry().cell();
        if observed.value().cell != cell {
            return Err(Error::Control("activation control changed Cell"));
        }
        if observed
            .value()
            .owner
            .as_ref()
            .is_none_or(|owner| owner.session != self.inner.session)
        {
            return Err(Error::Fenced);
        }
        Ok(cell)
    }

    pub(crate) fn ensure_running(&self) -> crate::Result<()> {
        if self.inner.shutting_down.load(Ordering::Acquire) {
            return Err(Error::RuntimeClosed);
        }
        self.inner.node_lease.check()
    }

    fn ensure_acquiring(&self) -> crate::Result<()> {
        self.ensure_running()?;
        if !self.inner.accepting_cells.load(Ordering::Acquire) {
            return Err(Error::CellDraining);
        }
        Ok(())
    }

    async fn activate_inner(
        &self,
        catalog: CatalogProof,
        activation: Activation,
        replica: crab_ltx::CellReplica,
        authority: CellAuthority,
        observed: VersionedControl,
    ) -> crate::Result<CellHandle> {
        let cell = self.activation_cell(&catalog, &observed)?;
        let incarnation = observed.value().incarnation;
        let code = observed.value().code;
        let schema = observed.value().schema;
        let scratch_directory = match &activation {
            Activation::Restored(activation) => activation.destination.parent(),
            Activation::Bootstrap(activation) => activation.destination.parent(),
        }
        .ok_or(Error::Control("Cell activation destination has no parent"))?
        .to_owned();
        let replica = self.replica_with_directory_cache(replica, &scratch_directory)?;
        let activation = match activation {
            Activation::Bootstrap(mut bootstrap) => {
                // The worker's new Db must use the same host admission and
                // filesystem as the publisher that confirms its captured cuts.
                bootstrap.replica = replica.clone();
                Activation::Bootstrap(bootstrap)
            }
            restored => restored,
        };
        let (reply, response) = oneshot::channel();
        let mut publisher = CellPublisher::new(replica, authority, observed, scratch_directory);
        if let Some(node_lease) = self.inner.node_lease.guard()? {
            publisher = publisher.with_node_lease(node_lease);
        }
        publisher = publisher.with_node_durability_slot(Arc::clone(&self.inner.node_durability));
        publisher = publisher.with_telemetry(self.inner.telemetry.clone());
        self.inner
            .sender
            .send(Message::Activate {
                cell,
                role: catalog.entry().role(),
                catalog: catalog.clone(),
                activation,
                publisher: Box::new(publisher),
                reply,
            })
            .await
            .map_err(|_| Error::RuntimeClosed)?;
        let admission = response.await.map_err(|_| Error::RuntimeClosed)??;
        Ok(CellHandle {
            cell,
            incarnation,
            code,
            schema,
            catalog,
            inner: self.inner.clone(),
            admission,
        })
    }

    fn replica_with_directory_cache(
        &self,
        replica: crab_ltx::CellReplica,
        destination: &Path,
    ) -> crate::Result<crab_ltx::CellReplica> {
        let scratch_directory = destination
            .parent()
            .ok_or(Error::Control("Cell activation destination has no parent"))?;
        let host = self
            .inner
            .replica_host
            .clone()
            .with_directory_cache(scratch_directory.join(".crab-cell-directory-cache"));
        Ok(replica.with_host(host))
    }
}
