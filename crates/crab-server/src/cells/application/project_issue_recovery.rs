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
        let receipt = fence(
            context,
            issue.repository,
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

async fn fence(
    context: &Context,
    repository: Uuid,
    input: source::FenceInput,
) -> Result<source::Outcome> {
    let target = Address::Repository(repository);
    let now_ms = crate::cells::unix_now_ms()?;
    let request = Request {
        target,
        operation: source::Fence::ID,
        mutation: Some(MutationIdentity {
            request_id: Uuid::now_v7(),
            issued_at_ms: now_ms,
            expires_at_ms: now_ms.saturating_add(60_000),
        }),
        minimum: None,
        input: serde_json::to_value(input)?,
    };
    // Use the ordinary native Cell call and output gate. A source decision is
    // safe to link only after the repository reply crosses that durability gate.
    let response = context
        .namespace(target.namespace())
        .map_err(Error::sdk)?
        .get_by_name(&target.name())
        .fetch(
            context,
            cellar::sdk::Request {
                url: format!("http://crab{}", target.path()),
                method: "POST".into(),
                headers: vec![("content-type".into(), "application/json".into())],
                body: cellar::engine_api::RequestBody::Bytes(serde_json::to_vec(&request)?.into()),
                entrypoint: None,
            },
        )
        .await
        .map_err(Error::sdk)?;
    if response.status != 200 || response.stream.is_some() || response.body.len() > MAX_RESULT_BYTES
    {
        return Err(Error::Unavailable);
    }
    let reply: Reply = serde_json::from_slice(&response.body)?;
    Ok(serde_json::from_value(reply.output)?)
}
