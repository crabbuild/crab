use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use crab_cell_app::ApplicationHandle;
use crab_cell_runtime::{
    ApplicationId, CellTarget, Error, MutationIdentity, Resolution, Result, SqlBatch,
    SqlBatchCommand, SqlResultSet, SqlStatement, SqlValue, TenantId, partition_for_shard,
};
use tokio::sync::Notify;

use crate::{
    fixture,
    qualification_cancellation::{cancel_prepared, committed_output},
};

pub async fn run(
    peer: &ApplicationHandle<fixture::ReferenceApplication>,
    observer: &ApplicationHandle<fixture::ReferenceApplication>,
    entered: Arc<Notify>,
    dispatched: &Arc<AtomicUsize>,
    tenant: TenantId,
    application: ApplicationId,
    row_id: i64,
    nonce: u64,
    mutation: MutationIdentity,
) -> Result<()> {
    let target = CellTarget::new(
        tenant,
        application,
        fixture::SQL_NAMESPACE,
        &partition_for_shard(0),
    )?;
    let expected = SqlValue::Blob(nonce.to_be_bytes().to_vec());
    let prepared = peer
        .prepare_command::<SqlBatchCommand<fixture::ReferenceSql>>(
            &target,
            mutation,
            SqlBatch {
                statements: vec![SqlStatement {
                    sql: "INSERT INTO qualification_rows (id, payload) VALUES (?1, ?2)".into(),
                    parameters: vec![SqlValue::Integer(row_id), expected.clone()],
                }],
            },
        )
        .await
        .map_err(|source| Error::Facility {
            name: "public qualification SQL cancellation prepare",
            source: Box::new(source),
        })?;
    let (_retained_attempt, resolution) =
        cancel_prepared(prepared, observer, entered, dispatched, true).await;
    let Resolution::Committed(outcome) = resolution else {
        return Err(Error::Control(
            "public qualification SQL cancelled acknowledgement unresolved",
        ));
    };
    let (output, commit_sequence) = committed_output::<Vec<SqlResultSet>>(outcome);
    if output.len() != 1 || output[0].rows_affected != 1 || dispatched.load(Ordering::Acquire) != 1
    {
        return Err(Error::Control(
            "public qualification SQL cancelled mutation differs",
        ));
    }
    let observed = observer
        .sql::<fixture::ReferenceSql>(target)?
        .query(
            None,
            SqlBatch {
                statements: vec![SqlStatement {
                    sql: "SELECT payload FROM qualification_rows WHERE id = ?1".into(),
                    parameters: vec![SqlValue::Integer(row_id)],
                }],
            },
        )
        .await
        .map_err(|source| Error::Facility {
            name: "public qualification SQL cancellation observation",
            source: Box::new(source),
        })?;
    if observed.output.len() != 1
        || observed.output[0].rows != vec![vec![expected]]
        || observed.receipt.commit_sequence < commit_sequence
    {
        return Err(Error::Control(
            "public qualification SQL cancelled row differs",
        ));
    }
    Ok(())
}
