use std::{
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

use crab_cell_app::ApplicationHandle;
use crab_cell_runtime::{
    ApplicationId, CellTarget, CronCommand, CronMutation, CronMutationOutcome, CronQueryResult,
    Error, InvocationError, KvAtomicCommand, KvAtomicOutcome, KvAtomicRequest, KvMutation,
    Resolution, Result, SqlBatch, SqlBatchCommand, SqlStatement, SqlValue, TenantId,
    partition_for_shard,
};
use tokio::sync::Notify;

use crate::{
    fixture,
    qualification::{fixed_id, identity},
    qualification_cancellation::now_ms,
};

pub struct ExpiryCase<'a> {
    writer: &'a ApplicationHandle<fixture::ReferenceApplication>,
    peer: &'a ApplicationHandle<fixture::ReferenceApplication>,
    observer: &'a ApplicationHandle<fixture::ReferenceApplication>,
    entered: &'a Notify,
    release: &'a Notify,
    dispatched: &'a AtomicUsize,
    tenant: TenantId,
    application: ApplicationId,
}

impl<'a> ExpiryCase<'a> {
    pub fn new(
        writer: &'a ApplicationHandle<fixture::ReferenceApplication>,
        peer: &'a ApplicationHandle<fixture::ReferenceApplication>,
        observer: &'a ApplicationHandle<fixture::ReferenceApplication>,
        entered: &'a Notify,
        release: &'a Notify,
        dispatched: &'a AtomicUsize,
        tenant: TenantId,
        application: ApplicationId,
    ) -> Self {
        Self {
            writer,
            peer,
            observer,
            entered,
            release,
            dispatched,
            tenant,
            application,
        }
    }

    pub async fn sql(self, operation_id: u64, nonce: u64) -> Result<()> {
        let Self {
            writer,
            peer,
            observer,
            entered,
            release,
            dispatched,
            tenant,
            application,
        } = self;
        let target = CellTarget::new(
            tenant,
            application,
            fixture::SQL_NAMESPACE,
            &partition_for_shard(0),
        )?;
        let row_id = i64::try_from(operation_id)
            .map_err(|_| Error::Control("public scheduled SQL row ID overflow"))?;
        let delayed_row_id = row_id.saturating_add(1);
        let payload = SqlValue::Blob(nonce.to_be_bytes().to_vec());
        let insert = |id| SqlBatch {
            statements: vec![SqlStatement {
                sql: "INSERT INTO qualification_rows (id, payload) VALUES (?1, ?2)".into(),
                parameters: vec![SqlValue::Integer(id), payload.clone()],
            }],
        };
        let observe = |id| SqlBatch {
            statements: vec![SqlStatement {
                sql: "SELECT payload FROM qualification_rows WHERE id = ?1".into(),
                parameters: vec![SqlValue::Integer(id)],
            }],
        };
        let writer_sql = writer.sql::<fixture::ReferenceSql>(target.clone())?;
        let observer_sql = observer.sql::<fixture::ReferenceSql>(target.clone())?;
        let acknowledged = writer_sql
            .batch(
                identity(operation_id.saturating_mul(100), now_ms()),
                insert(row_id),
            )
            .await
            .map_err(|source| Error::Facility {
                name: "public scheduled SQL expiry acknowledged write",
                source: Box::new(source),
            })?;
        if acknowledged.output.len() != 1 || acknowledged.output[0].rows_affected != 1 {
            return Err(Error::Control(
                "public scheduled SQL acknowledged write differs",
            ));
        }
        let before = observer_sql
            .query(Some(acknowledged.receipt), observe(row_id))
            .await
            .map_err(|source| Error::Facility {
                name: "public scheduled SQL pre-expiry observation",
                source: Box::new(source),
            })?;
        if before.output.len() != 1 || before.output[0].rows != vec![vec![payload.clone()]] {
            return Err(Error::Control(
                "public scheduled SQL acknowledged row missing",
            ));
        }

        let issued_at_ms = now_ms();
        let expires_at_ms = issued_at_ms.saturating_add(15_000);
        let mut expired_identity = identity(
            operation_id.saturating_mul(100).saturating_add(1),
            issued_at_ms,
        );
        expired_identity.expires_at_ms = expires_at_ms;
        let prepared = peer
            .prepare_command::<SqlBatchCommand<fixture::ReferenceSql>>(
                &target,
                expired_identity,
                insert(delayed_row_id),
            )
            .await
            .map_err(|source| Error::Facility {
                name: "public scheduled SQL expiring command prepare",
                source: Box::new(source),
            })?;
        let evidence = prepared.evidence().clone();
        let delayed = tokio::spawn(async move { prepared.execute().await });
        tokio::time::timeout(Duration::from_secs(20), entered.notified())
            .await
            .map_err(|_| Error::Control("public scheduled SQL signed receive not reached"))?;
        if dispatched.load(Ordering::Acquire) != 0 {
            return Err(Error::Control(
                "public scheduled SQL expiry dispatched early",
            ));
        }
        let absent = observer_sql
            .query(None, observe(delayed_row_id))
            .await
            .map_err(|source| Error::Facility {
                name: "public scheduled SQL delayed row observation",
                source: Box::new(source),
            })?;
        if absent.output.len() != 1 || !absent.output[0].rows.is_empty() {
            return Err(Error::Control("public scheduled SQL delayed row appeared"));
        }
        let remaining_ms = expires_at_ms
            .saturating_sub(now_ms())
            .max(0)
            .saturating_add(100);
        tokio::time::sleep(Duration::from_millis(u64::try_from(remaining_ms).map_err(
            |_| Error::Control("public scheduled SQL expiry wait overflow"),
        )?))
        .await;
        release.notify_one();
        let outcome = delayed.await.map_err(|source| Error::Facility {
            name: "public scheduled SQL delayed command join",
            source: Box::new(source),
        })?;
        if !matches!(
            outcome,
            Err(InvocationError::NotStarted(Error::Peer(
                "invalid or expired mutation identity"
            )))
        ) {
            return Err(Error::Control(
                "public scheduled SQL expired identity was accepted",
            ));
        }
        if dispatched.load(Ordering::Acquire) != 0 {
            return Err(Error::Control(
                "public scheduled SQL expired command dispatched",
            ));
        }
        if observer
            .resolve(&evidence)
            .await
            .map_err(|source| Error::Facility {
                name: "public scheduled SQL expiry resolution",
                source: Box::new(source),
            })?
            != Resolution::Expired
        {
            return Err(Error::Control("public scheduled SQL identity not expired"));
        }
        let after = observer_sql
            .query(None, observe(row_id))
            .await
            .map_err(|source| Error::Facility {
                name: "public scheduled SQL acknowledged row after expiry",
                source: Box::new(source),
            })?;
        let rejected = observer_sql
            .query(None, observe(delayed_row_id))
            .await
            .map_err(|source| Error::Facility {
                name: "public scheduled SQL rejected row observation",
                source: Box::new(source),
            })?;
        if after.output.len() != 1
            || after.output[0].rows != vec![vec![payload]]
            || after.receipt.commit_sequence < acknowledged.receipt.commit_sequence
            || rejected.output.len() != 1
            || !rejected.output[0].rows.is_empty()
        {
            return Err(Error::Control("public scheduled SQL expiry state differs"));
        }
        Ok(())
    }

    pub async fn cron(self, operation_id: u64, nonce: u64) -> Result<()> {
        let Self {
            writer,
            peer,
            observer,
            entered,
            release,
            dispatched,
            tenant,
            application,
        } = self;
        let target = CellTarget::new(
            tenant,
            application,
            fixture::CRON_NAMESPACE,
            &partition_for_shard(0),
        )?;
        let schedule_id = fixed_id(operation_id);
        let payload = nonce.to_be_bytes().to_vec();
        let next_due_ms = now_ms().saturating_add(300_000);
        let writer_cron = writer.cron::<fixture::ReferenceCron>()?;
        let observer_cron = observer.cron::<fixture::ReferenceCron>()?;
        let acknowledged = writer_cron
            .mutate(
                identity(operation_id.saturating_mul(100), now_ms()),
                CronMutation::Upsert {
                    schedule_id,
                    target_index: 0,
                    target_partition: partition_for_shard(0).to_vec(),
                    payload: payload.clone(),
                    interval_ms: 60_000,
                    next_due_ms,
                },
            )
            .await
            .map_err(|source| Error::Facility {
                name: "public scheduled Cron expiry acknowledged upsert",
                source: Box::new(source),
            })?;
        let CronMutationOutcome::Applied { generation } = acknowledged.output else {
            return Err(Error::Control(
                "public scheduled Cron acknowledged upsert differs",
            ));
        };
        let before = observer_cron
            .get(schedule_id, Some(acknowledged.receipt))
            .await
            .map_err(|source| Error::Facility {
                name: "public scheduled Cron pre-expiry observation",
                source: Box::new(source),
            })?;
        let CronQueryResult::Get(Some(schedule)) = before.output else {
            return Err(Error::Control("public scheduled Cron schedule missing"));
        };
        if schedule.schedule_id != schedule_id
            || schedule.payload != payload
            || schedule.generation != generation
            || schedule.next_due_ms != next_due_ms
            || schedule.occurrence != 0
            || !schedule.enabled
        {
            return Err(Error::Control("public scheduled Cron schedule differs"));
        }

        let issued_at_ms = now_ms();
        let expires_at_ms = issued_at_ms.saturating_add(15_000);
        let mut expired_identity = identity(
            operation_id.saturating_mul(100).saturating_add(1),
            issued_at_ms,
        );
        expired_identity.expires_at_ms = expires_at_ms;
        let prepared = peer
            .prepare_command::<CronCommand<fixture::ReferenceCron>>(
                &target,
                expired_identity,
                CronMutation::Pause { schedule_id },
            )
            .await
            .map_err(|source| Error::Facility {
                name: "public scheduled Cron expiring command prepare",
                source: Box::new(source),
            })?;
        let evidence = prepared.evidence().clone();
        let delayed = tokio::spawn(async move { prepared.execute().await });
        tokio::time::timeout(Duration::from_secs(20), entered.notified())
            .await
            .map_err(|_| Error::Control("public scheduled Cron signed receive not reached"))?;
        if dispatched.load(Ordering::Acquire) != 0 {
            return Err(Error::Control(
                "public scheduled Cron expiry dispatched early",
            ));
        }
        let pending = observer_cron
            .get(schedule_id, None)
            .await
            .map_err(|source| Error::Facility {
                name: "public scheduled Cron delayed mutation observation",
                source: Box::new(source),
            })?;
        if pending.output != CronQueryResult::Get(Some(schedule.clone())) {
            return Err(Error::Control(
                "public scheduled Cron delayed mutation appeared",
            ));
        }
        let remaining_ms = expires_at_ms
            .saturating_sub(now_ms())
            .max(0)
            .saturating_add(100);
        tokio::time::sleep(Duration::from_millis(u64::try_from(remaining_ms).map_err(
            |_| Error::Control("public scheduled Cron expiry wait overflow"),
        )?))
        .await;
        release.notify_one();
        let outcome = delayed.await.map_err(|source| Error::Facility {
            name: "public scheduled Cron delayed command join",
            source: Box::new(source),
        })?;
        if !matches!(
            outcome,
            Err(InvocationError::NotStarted(Error::Peer(
                "invalid or expired mutation identity"
            )))
        ) {
            return Err(Error::Control(
                "public scheduled Cron expired identity was accepted",
            ));
        }
        if dispatched.load(Ordering::Acquire) != 0 {
            return Err(Error::Control(
                "public scheduled Cron expired command dispatched",
            ));
        }
        if observer
            .resolve(&evidence)
            .await
            .map_err(|source| Error::Facility {
                name: "public scheduled Cron expiry resolution",
                source: Box::new(source),
            })?
            != Resolution::Expired
        {
            return Err(Error::Control("public scheduled Cron identity not expired"));
        }
        let after = observer_cron
            .get(schedule_id, None)
            .await
            .map_err(|source| Error::Facility {
                name: "public scheduled Cron post-expiry observation",
                source: Box::new(source),
            })?;
        if after.output != CronQueryResult::Get(Some(schedule))
            || after.receipt.commit_sequence < acknowledged.receipt.commit_sequence
        {
            return Err(Error::Control("public scheduled Cron expiry state differs"));
        }
        Ok(())
    }

    pub async fn kv(self, operation_id: u64, nonce: u64) -> Result<()> {
        let Self {
            writer,
            peer,
            observer,
            entered,
            release,
            dispatched,
            tenant,
            application,
        } = self;
        let scope = b"public-qualification-scheduled-expiry".to_vec();
        let ttl_key = fixed_id(operation_id).to_vec();
        let delayed_key = fixed_id(operation_id.saturating_add(1)).to_vec();
        let payload = nonce.to_be_bytes().to_vec();
        let expires_at_ms = now_ms().saturating_add(15_000);
        let writer_kv = writer.kv::<fixture::ReferenceKv>(fixture::KV_NAMESPACE)?;
        let observer_kv = observer.kv::<fixture::ReferenceKv>(fixture::KV_NAMESPACE)?;
        let inserted = writer_kv
            .atomic(
                identity(operation_id.saturating_mul(100), now_ms()),
                KvAtomicRequest {
                    scope: scope.clone(),
                    checks: Vec::new(),
                    mutations: vec![KvMutation::Put {
                        key: ttl_key.clone(),
                        value: payload.clone(),
                        expires_at_ms: Some(expires_at_ms),
                    }],
                },
            )
            .await
            .map_err(|source| Error::Facility {
                name: "public scheduled KV expiry write",
                source: Box::new(source),
            })?;
        let KvAtomicOutcome::Applied(results) = inserted.output else {
            return Err(Error::Control("public scheduled KV TTL write not applied"));
        };
        let [result] = results.as_slice() else {
            return Err(Error::Control("public scheduled KV TTL result differs"));
        };
        let Some(version) = result.version else {
            return Err(Error::Control("public scheduled KV TTL version missing"));
        };
        let before = observer_kv
            .get(scope.clone(), ttl_key.clone(), Some(inserted.receipt))
            .await
            .map_err(|source| Error::Facility {
                name: "public scheduled KV pre-expiry observation",
                source: Box::new(source),
            })?;
        if !matches!(before.output, Some(ref entry) if entry.value == payload && entry.version == version)
        {
            return Err(Error::Control("public scheduled KV TTL value missing"));
        }

        let target = CellTarget::new(
            tenant,
            application,
            fixture::KV_NAMESPACE,
            &partition_for_shard(0),
        )?;
        let issued_at_ms = now_ms();
        let mut expired_identity = identity(
            operation_id.saturating_mul(100).saturating_add(1),
            issued_at_ms,
        );
        expired_identity.expires_at_ms = expires_at_ms;
        let prepared = peer
            .prepare_command::<KvAtomicCommand<fixture::ReferenceKv>>(
                &target,
                expired_identity,
                KvAtomicRequest {
                    scope: scope.clone(),
                    checks: Vec::new(),
                    mutations: vec![KvMutation::Put {
                        key: delayed_key.clone(),
                        value: payload,
                        expires_at_ms: None,
                    }],
                },
            )
            .await
            .map_err(|source| Error::Facility {
                name: "public scheduled KV expiring command prepare",
                source: Box::new(source),
            })?;
        let evidence = prepared.evidence().clone();
        let delayed = tokio::spawn(async move { prepared.execute().await });
        tokio::time::timeout(Duration::from_secs(20), entered.notified())
            .await
            .map_err(|_| Error::Control("public scheduled KV signed receive not reached"))?;
        if dispatched.load(Ordering::Acquire) != 0 {
            return Err(Error::Control(
                "public scheduled KV expiry dispatched early",
            ));
        }
        let absent = observer_kv
            .get(scope.clone(), delayed_key.clone(), None)
            .await
            .map_err(|source| Error::Facility {
                name: "public scheduled KV delayed mutation observation",
                source: Box::new(source),
            })?;
        if absent.output.is_some() {
            return Err(Error::Control(
                "public scheduled KV delayed mutation appeared",
            ));
        }
        let remaining_ms = expires_at_ms
            .saturating_sub(now_ms())
            .max(0)
            .saturating_add(100);
        tokio::time::sleep(Duration::from_millis(
            u64::try_from(remaining_ms)
                .map_err(|_| Error::Control("public scheduled KV expiry wait overflow"))?,
        ))
        .await;
        release.notify_one();
        let outcome = delayed.await.map_err(|source| Error::Facility {
            name: "public scheduled KV delayed command join",
            source: Box::new(source),
        })?;
        if !matches!(
            outcome,
            Err(InvocationError::NotStarted(Error::Peer(
                "invalid or expired mutation identity"
            )))
        ) {
            return Err(Error::Control(
                "public scheduled KV expired identity was accepted",
            ));
        }
        if dispatched.load(Ordering::Acquire) != 0 {
            return Err(Error::Control(
                "public scheduled KV expired command dispatched",
            ));
        }
        let resolution = observer
            .resolve(&evidence)
            .await
            .map_err(|source| Error::Facility {
                name: "public scheduled KV expiry resolution",
                source: Box::new(source),
            })?;
        if resolution != Resolution::Expired {
            return Err(Error::Control("public scheduled KV identity not expired"));
        }
        let expired = observer_kv
            .get(scope.clone(), ttl_key, None)
            .await
            .map_err(|source| Error::Facility {
                name: "public scheduled KV post-expiry observation",
                source: Box::new(source),
            })?;
        let delayed_absent = observer_kv
            .get(scope, delayed_key, None)
            .await
            .map_err(|source| Error::Facility {
                name: "public scheduled KV rejected write observation",
                source: Box::new(source),
            })?;
        if expired.output.is_some() || delayed_absent.output.is_some() {
            return Err(Error::Control("public scheduled KV expiry state differs"));
        }
        Ok(())
    }
}
