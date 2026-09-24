//! Activation, bootstrap publication, and failure cleanup for one Cell.

use super::*;

pub(in crate::cell::actor) async fn activate_restored_and_publish(
    cell: CellId,
    pool: &SqlWorkerPool,
    publisher: &mut CellPublisher,
    activation: RestoredActivation,
) -> crate::Result<Option<crab_ltx::Hydration>> {
    let RestoredActivation {
        database,
        destination,
        incarnation,
        schema,
        root,
        reservation,
    } = activation;
    pool.activate_restored(
        cell,
        database,
        destination,
        incarnation,
        schema,
        root,
        reservation,
    )
    .await?;
    if let Err(error) = publisher.activate().await {
        return match pool.deactivate(cell).await {
            Ok(()) => Err(error),
            Err(cleanup) => Err(cleanup),
        };
    }
    pool.hydration(cell).await
}

pub(in crate::cell::actor) async fn bootstrap_and_publish(
    cell: CellId,
    pool: &SqlWorkerPool,
    publisher: &mut CellPublisher,
    activation: BootstrapActivation,
) -> crate::Result<Option<crab_ltx::Hydration>> {
    let BootstrapActivation {
        replica,
        destination,
        incarnation,
        schema,
        initialize,
        reservation,
    } = activation;
    let bootstrap = pool.bootstrap(
        cell,
        replica,
        destination,
        incarnation,
        schema,
        initialize,
        reservation,
    );
    tokio::pin!(bootstrap);
    let mut renewal_error = None;
    let bootstrap = loop {
        tokio::select! {
            result = &mut bootstrap => break result,
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(publisher.renewal_at())) => {
                if let Err(error) = publisher.renew().await {
                    renewal_error = Some(error);
                    break bootstrap.await;
                }
            }
        }
    };
    if let Some(error) = renewal_error {
        return match bootstrap {
            Ok(_) => match pool.deactivate(cell).await {
                Ok(()) => Err(error),
                Err(cleanup) => Err(cleanup),
            },
            Err(bootstrap) => Err(bootstrap),
        };
    }
    let bootstrap = bootstrap?;
    let publication = async {
        let prepared = publisher.prepare_initial(&bootstrap.cuts).await?;
        publisher
            .publish_prepared(&prepared, bootstrap.next_due_ms)
            .await?;
        pool.confirm_bootstrap_published(cell, bootstrap.cuts)
            .await?;
        Ok(())
    }
    .await;
    if let Err(error) = publication {
        return match pool.deactivate(cell).await {
            Ok(()) => Err(error),
            Err(cleanup) => Err(cleanup),
        };
    }
    Ok(None)
}

pub(in crate::cell::actor) async fn cleanup_failed_activation(
    cell: CellId,
    pool: &SqlWorkerPool,
    publisher: &mut CellPublisher,
) -> crate::Result<()> {
    match pool.deactivate(cell).await {
        Ok(()) | Err(Error::CellNotActive) => {}
        Err(error) => return Err(error),
    }
    if publisher.control().value().root.is_some() {
        publisher.release().await
    } else {
        Ok(())
    }
}

pub(in crate::cell::actor) async fn rollback_failed_acquisition(
    authority: &CellAuthority,
    claimed: &VersionedControl,
    replica: &crab_ltx::CellReplica,
    node_lease: Option<NodeLeaseGuard>,
) -> crate::Result<()> {
    let current = authority
        .load(claimed.value().cell)
        .await?
        .ok_or(Error::Fenced)?;
    if current.value().state == crate::control::ControlState::Idle
        && current.value().owner.is_none()
    {
        return Ok(());
    }
    if current.value().epoch != claimed.value().epoch
        || current.value().owner != claimed.value().owner
        || current.value().root != claimed.value().root
        || current.value().recovery != claimed.value().recovery
        || current.value().code != claimed.value().code
        || current.value().schema != claimed.value().schema
        || current.value().recovery.is_some()
    {
        return Ok(());
    }
    let mut publisher =
        CellPublisher::new(replica.clone(), authority.clone(), current, PathBuf::new());
    if let Some(node_lease) = node_lease {
        publisher = publisher.with_node_lease(node_lease);
    }
    match publisher.release().await {
        Ok(_) => Ok(()),
        Err(error) => {
            let latest = authority
                .load(claimed.value().cell)
                .await?
                .ok_or(Error::Fenced)?;
            if (latest.value().state == crate::control::ControlState::Idle
                && latest.value().owner.is_none())
                || latest.value().epoch != claimed.value().epoch
                || latest.value().owner != claimed.value().owner
                || latest.value().root != claimed.value().root
                || latest.value().recovery != claimed.value().recovery
                || latest.value().code != claimed.value().code
                || latest.value().schema != claimed.value().schema
            {
                Ok(())
            } else {
                Err(error)
            }
        }
    }
}
