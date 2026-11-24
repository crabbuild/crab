use super::*;
use crate::cells::repository::issues::transfer::publication::{
    self, Decision, Fence, Publish, PublishOutcome, Recovery, Reservation,
};

pub(super) async fn alarm(context: &Context) -> Result<()> {
    let storage = context.storage().map_err(Error::sdk)?;
    let now_ms = crate::cells::unix_now_ms()?;
    let work = storage
        .transaction_sync(|storage| Ok(publication::due(&Transaction { storage, now_ms })?))
        .map_err(Error::sdk)?;
    let result = match work {
        Some(Recovery::Publish(decision)) => publish(context, &storage, decision).await,
        Some(Recovery::Fence(reservation)) => fence(context, &storage, reservation).await,
        None => return Ok(()),
    };
    if let Err(error) = result {
        tracing::warn!(%error, "Issue transfer recovery will be retried");
    }
    Ok(())
}

async fn publish(context: &Context, storage: &Storage, decision: Decision) -> Result<()> {
    let destination = decision.destination();
    let outcome = command_from_alarm::<Publish>(
        context,
        Address::Repository(destination.repository),
        decision.clone(),
    )
    .await?;
    if outcome != (PublishOutcome::Published { destination }) {
        return Err(Error::Invalid(
            "Issue transfer publication decision conflicts",
        ));
    }
    let now_ms = crate::cells::unix_now_ms()?;
    storage
        .transaction_sync(|storage| {
            publication::acknowledged(&Transaction { storage, now_ms }, &decision)?;
            Ok(())
        })
        .map_err(Error::sdk)
}

async fn fence(context: &Context, storage: &Storage, reservation: Reservation) -> Result<()> {
    let outcome = command_from_alarm::<Fence>(
        context,
        Address::Repository(reservation.source.repository),
        reservation.clone(),
    )
    .await?;
    let now_ms = crate::cells::unix_now_ms()?;
    storage
        .transaction_sync(|storage| {
            publication::fenced(&Transaction { storage, now_ms }, &reservation, outcome)?;
            Ok(())
        })
        .map_err(Error::sdk)
}
