use std::{
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

use crab_cell_app::ApplicationHandle;
use crab_cell_runtime::{
    ApplicationId, CellTarget, Error, InvocationError, KvAtomicCommand, KvAtomicOutcome,
    KvAtomicRequest, KvMutation, Resolution, Result, TenantId, partition_for_shard,
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
