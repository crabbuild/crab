use super::*;
use crate::cells::repository::issues::transfer::publication::{self, Publish, PublishOutcome};

pub(super) async fn alarm(context: &Context) -> Result<()> {
    let storage = context.storage().map_err(Error::sdk)?;
    let now_ms = crate::cells::unix_now_ms()?;
    let decision = storage
        .transaction_sync(|storage| Ok(publication::due(&Transaction { storage, now_ms })?))
        .map_err(Error::sdk)?;
    let Some(decision) = decision else {
        return Ok(());
    };
    let destination = decision.destination();
    let result = async {
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
    .await;
    if let Err(error) = result {
        tracing::warn!(repository_id = %destination.repository, issue = destination.number, %error, "Issue transfer publication will be retried");
    }
    Ok(())
}
