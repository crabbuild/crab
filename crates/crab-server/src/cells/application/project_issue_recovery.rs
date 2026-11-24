use super::*;
use crate::cells::repository::projects::organization_issue as source;

pub(super) async fn alarm(context: &Context) -> Result<()> {
    let organization = context
        .identity()
        .and_then(|identity| identity.name.as_deref())
        .and_then(|name| Uuid::parse_str(name).ok())
        .filter(|id| !id.is_nil())
        .ok_or(Error::Invalid(
            "Project recovery has no organization Cell address",
        ))?;
    let storage = context.storage().map_err(Error::sdk)?;
    storage
        .sql_cursor("PRAGMA foreign_keys = ON", &[])
        .map_err(Error::sdk)?;
    let now_ms = crate::cells::unix_now_ms()?;
    let issue = storage
        .transaction_sync(|storage| {
            Ok(organization::projects::due(&Transaction {
                storage,
                now_ms,
            })?)
        })
        .map_err(Error::sdk)?;
    let Some(issue) = issue else { return Ok(()) };
    let result = async {
        let receipt = command_from_alarm::<source::Fence>(
            context,
            Address::Repository(issue.repository),
            source::FenceInput {
                organization,
                request_id: issue.request_id,
                digest: issue.digest,
            },
        )
        .await?;
        let now_ms = crate::cells::unix_now_ms()?;
        storage
            .transaction_sync(|storage| {
                organization::projects::ResolveIssue::execute(
                    &mut Transaction { storage, now_ms },
                    organization::projects::ResolveInput {
                        request_id: issue.request_id,
                        digest: issue.digest,
                        receipt,
                    },
                )?;
                Ok(())
            })
            .map_err(Error::sdk)
    }
    .await;
    if let Err(error) = result {
        // Selecting the job already armed its next attempt. The current run
        // can release the alarm without dropping this accepted reservation.
        tracing::warn!(%organization, request_id = %issue.request_id, %error, "Project issue recovery will be retried");
    }
    Ok(())
}
