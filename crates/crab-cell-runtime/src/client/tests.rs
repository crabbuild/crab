use std::{
    future::Future,
    pin::Pin,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicUsize, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::sync::Notify;

use super::{
    CellClient, CellDescription, CellTransport, EncodedCommand, EncodedObservation, EncodedQuery,
    EncodedResolve, InvocationError, PendingMutation, decode_pending,
};
use crate::Error;
use crate::cell::catalog::CatalogRole;
use crate::cell::executor::StoredOutcome;
use crate::cell::executor::{MutationIdentity, Resolution};
use crate::client::Receipt;
use crate::identity::{ApplicationId, CellTarget, Digest, NamespaceId, SessionId, TenantId};
use crate::identity::{IncarnationId, RequestId};
use crate::peer::{PeerPrincipal, PeerRoundTrip, PeerSigner};
use crate::registry::{
    BuildDescriptor, CellModule, Command, MigrationDescriptor, ModuleDescriptor,
    NamespaceDescriptor, Query, RegistryBuilder,
};
use crate::registry::{
    CommandContext, CommandResult, OperationDescriptor, QueryContext, RetainedCodeDescriptor,
};

const MODULE: &str = "pending-test";
const ADMISSION_CELL: crate::CellId = crate::CellId::from_bytes([1; 32]);
const NAMESPACE: NamespaceId = NamespaceId::from_bytes([3; 16]);
const MIGRATION: &str = "CREATE TABLE pending_test(value BLOB NOT NULL)";
const RETAINED_CODE: Digest = Digest::from_bytes([14; 32]);

#[test]
fn pending_rejection_preserves_published_receipt() {
    let target = CellTarget::new(
        TenantId::from_bytes([1; 16]),
        ApplicationId::from_bytes([2; 16]),
        NAMESPACE,
        b"pending",
    )
    .expect("valid pending target");
    let incarnation = IncarnationId::from_bytes([3; 16]);
    let pending = PendingMutation {
        target: target.clone(),
        incarnation,
        identity: MutationIdentity {
            request_id: RequestId::from_bytes([4; 16]),
            issued_at_ms: 1_000,
            expires_at_ms: 61_000,
        },
        operation_digest: Digest::from_bytes([5; 32]),
        max_result_bytes: 64,
    };
    let result =
        crate::codec::encode_wire(&b"lease-lost".to_vec(), 64).expect("bounded rejection result");

    let decoded = decode_pending::<Vec<u8>>(
        &pending,
        StoredOutcome::Rejected {
            result,
            commit_sequence: 42,
        },
    );

    assert!(matches!(
        decoded,
        Err(InvocationError::Rejected(committed))
            if committed.output == b"lease-lost"
                && committed.receipt == Receipt {
                    cell: target.cell_id(),
                    incarnation,
                    commit_sequence: 42,
                }
    ));
}

struct PendingCommand;

impl Command for PendingCommand {
    const MODULE: &'static str = MODULE;
    const ID: u32 = 1;
    const CODEC_VERSION: u32 = 1;
    type Input = Vec<u8>;
    type Output = Vec<u8>;

    fn execute(
        _context: &mut CommandContext<'_, '_>,
        input: Self::Input,
    ) -> crate::Result<CommandResult<Self::Output>> {
        Ok(CommandResult::Success(input))
    }
}

struct StreamQuery;

impl Query for StreamQuery {
    const MODULE: &'static str = MODULE;
    const ID: u32 = 2;
    const CODEC_VERSION: u32 = 1;
    type Input = u64;
    type Output = Vec<u8>;

    fn execute(_context: &mut QueryContext<'_>, input: Self::Input) -> crate::Result<Self::Output> {
        Ok(input.to_be_bytes().to_vec())
    }
}

struct AcceptedButLost {
    calls: Arc<AtomicUsize>,
}

impl PeerRoundTrip for AcceptedButLost {
    fn send(
        &self,
        _target: CellTarget,
        _request: Vec<u8>,
        _remaining_ms: u32,
    ) -> Pin<Box<dyn Future<Output = crate::Result<Vec<u8>>> + Send + 'static>> {
        let calls = Arc::clone(&self.calls);
        Box::pin(async move {
            calls.fetch_add(1, Ordering::SeqCst);
            Err(Error::PeerTransportUnknown {
                context: "test accepted request lost its response",
                source: Box::new(Error::RuntimeClosed),
            })
        })
    }
}

#[tokio::test]
async fn ambiguous_peer_command_preserves_identity_without_retry() {
    let target = CellTarget::new(
        TenantId::from_bytes([1; 16]),
        ApplicationId::from_bytes([2; 16]),
        NAMESPACE,
        b"pending",
    )
    .unwrap();
    let identity = MutationIdentity {
        request_id: RequestId::from_bytes([7; 16]),
        issued_at_ms: 1_000,
        expires_at_ms: 61_000,
    };
    let operation_digest = Digest::from_bytes([8; 32]);
    let calls = Arc::new(AtomicUsize::new(0));
    let transport = crate::peer::PeerClientTransport::new(
        Arc::new(PeerSigner::new(
            SessionId::from_bytes([9; 16]),
            Digest::from_bytes([10; 32]),
            ed25519_dalek::SigningKey::from_bytes(&[11; 32]),
        )),
        PeerPrincipal {
            issuer: "urn:crab:test".into(),
            subject: "operator".into(),
            actions: vec!["repository.write".into()],
        },
        Arc::new(AcceptedButLost {
            calls: Arc::clone(&calls),
        }),
    );

    let result = CellTransport::command(
        &transport,
        EncodedCommand {
            target: target.clone(),
            expected: CellDescription {
                cell: target.cell_id(),
                incarnation: IncarnationId::from_bytes([12; 16]),
                code: Digest::from_bytes([13; 32]),
                schema: 1,
            },
            identity,
            operation_digest,
            now_ms: 1_000,
            module: MODULE,
            operation_id: 1,
            codec_version: 1,
            input: b"input".to_vec(),
            input_limit: 64,
            output_limit: 64,
        },
    )
    .await;

    assert!(matches!(
        result,
        Err(Error::OutcomeUnknown {
            request_id,
            operation_digest: digest,
            ..
        }) if request_id == identity.request_id && digest == operation_digest
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

struct PendingModule;

impl CellModule for PendingModule {
    const NAME: &'static str = MODULE;

    fn descriptor(&self) -> &'static ModuleDescriptor {
        static DESCRIPTOR: OnceLock<ModuleDescriptor> = OnceLock::new();
        DESCRIPTOR.get_or_init(|| ModuleDescriptor {
            name: MODULE,
            source_digest: Digest::from_bytes([4; 32]),
            retained_codes: &[RetainedCodeDescriptor {
                code: RETAINED_CODE,
                schema_min: 1,
                schema_max: 1,
            }],
            schema_min: 1,
            schema_max: 1,
            migrations: Box::leak(Box::new([MigrationDescriptor {
                version: 1,
                sql: MIGRATION,
                digest: Digest::from_bytes(*blake3::hash(MIGRATION.as_bytes()).as_bytes()),
            }])),
            commands: &[OperationDescriptor {
                id: 1,
                codec_version: 1,
                schema_min: 1,
                schema_max: 1,
                input_limit: 64,
                output_limit: 64,
            }],
            queries: &[OperationDescriptor {
                id: 2,
                codec_version: 1,
                schema_min: 1,
                schema_max: 1,
                input_limit: 64,
                output_limit: 64,
            }],
            workflow_definitions: &[],
            activity_types: &[],
            namespaces: &[NamespaceDescriptor {
                id: NAMESPACE,
                name: MODULE,
                role: CatalogRole::Repository,
                shards: 1,
                effect_targets: &[],
                dead_letter: None,
            }],
        })
    }

    fn register(self, registry: &mut RegistryBuilder) -> crate::Result<()> {
        registry.bind_command::<PendingCommand>()?;
        registry.bind_query::<StreamQuery>()
    }
}

struct StreamTransport {
    description: CellDescription,
    sequence: Arc<AtomicUsize>,
    fenced: Arc<AtomicUsize>,
    query_started: Option<Arc<Notify>>,
    query_release: Option<Arc<Notify>>,
}

impl CellTransport for StreamTransport {
    fn describe(
        &self,
        _target: CellTarget,
    ) -> Pin<Box<dyn Future<Output = crate::Result<CellDescription>> + Send + 'static>> {
        let description = self.description;
        Box::pin(async move { Ok(description) })
    }

    fn command(
        &self,
        _command: EncodedCommand,
    ) -> Pin<Box<dyn Future<Output = crate::Result<StoredOutcome>> + Send + 'static>> {
        Box::pin(async { Err(Error::Command("unexpected stream command")) })
    }

    fn query(
        &self,
        _query: EncodedQuery,
    ) -> Pin<Box<dyn Future<Output = crate::Result<EncodedObservation>> + Send + 'static>> {
        let fenced = self.fenced.load(Ordering::Acquire) != 0;
        let description = self.description;
        let sequence = self.sequence.fetch_add(1, Ordering::SeqCst) as u64 + 1;
        let query_started = self.query_started.clone();
        let query_release = self.query_release.clone();
        Box::pin(async move {
            if let Some(query_started) = query_started {
                query_started.notify_one();
            }
            if let Some(query_release) = query_release {
                query_release.notified().await;
            }
            if fenced {
                return Err(Error::Fenced);
            }
            let output = crate::codec::encode_wire(&sequence.to_be_bytes().to_vec(), 64)?;
            Ok(EncodedObservation {
                output,
                receipt: Receipt {
                    cell: description.cell,
                    incarnation: description.incarnation,
                    commit_sequence: sequence,
                },
            })
        })
    }

    fn resolve(
        &self,
        _resolve: EncodedResolve,
    ) -> Pin<Box<dyn Future<Output = crate::Result<Resolution>> + Send + 'static>> {
        Box::pin(async { Err(Error::Command("unexpected stream resolve")) })
    }
}

struct RefusedThenUnknownTransport {
    description: CellDescription,
    descriptions: AtomicUsize,
    command_digest: Arc<Mutex<Option<Digest>>>,
    resolved_digest: Arc<Mutex<Option<Digest>>>,
    calls: Arc<AtomicUsize>,
}

impl CellTransport for RefusedThenUnknownTransport {
    fn describe(
        &self,
        _target: CellTarget,
    ) -> Pin<Box<dyn Future<Output = crate::Result<CellDescription>> + Send + 'static>> {
        let description = self.description;
        let refused = self.descriptions.fetch_add(1, Ordering::SeqCst) < 2;
        Box::pin(async move {
            if refused {
                Err(Error::Capacity("peer HTTP admission"))
            } else {
                Ok(description)
            }
        })
    }

    fn command(
        &self,
        command: EncodedCommand,
    ) -> Pin<Box<dyn Future<Output = crate::Result<StoredOutcome>> + Send + 'static>> {
        let observed = self.command_digest.clone();
        if self.calls.fetch_add(1, Ordering::SeqCst) < 2 {
            return Box::pin(async { Err(Error::Capacity("owner mailbox")) });
        }
        Box::pin(async move {
            *observed.lock().unwrap() = Some(command.operation_digest);
            Err(Error::OutcomeUnknown {
                request_id: command.identity.request_id,
                operation_digest: command.operation_digest,
                source: Box::new(Error::Capacity("accepted publication")),
            })
        })
    }

    fn query(
        &self,
        _query: EncodedQuery,
    ) -> Pin<Box<dyn Future<Output = crate::Result<EncodedObservation>> + Send + 'static>> {
        Box::pin(async { Err(Error::Command("unexpected query")) })
    }

    fn resolve(
        &self,
        resolve: EncodedResolve,
    ) -> Pin<Box<dyn Future<Output = crate::Result<Resolution>> + Send + 'static>> {
        let observed = self.resolved_digest.clone();
        Box::pin(async move {
            *observed.lock().unwrap() = Some(resolve.operation_digest);
            Ok(Resolution::Unknown)
        })
    }
}

#[tokio::test]
async fn capacity_wait_preserves_unknown_outcome_identity_and_digest_for_resolve() {
    let mut builder = RegistryBuilder::new(BuildDescriptor {
        source_revision: "pending-test".into(),
        cargo_lock_digest: Digest::from_bytes([5; 32]),
    });
    builder.register(PendingModule).unwrap();
    let registry = Arc::new(builder.finish().unwrap());
    let target = CellTarget::new(
        TenantId::from_bytes([1; 16]),
        ApplicationId::from_bytes([2; 16]),
        NAMESPACE,
        b"pending",
    )
    .unwrap();
    let description = CellDescription {
        cell: target.cell_id(),
        incarnation: IncarnationId::from_bytes([6; 16]),
        code: RETAINED_CODE,
        schema: 1,
    };
    let command_digest = Arc::new(Mutex::new(None));
    let resolved_digest = Arc::new(Mutex::new(None));
    let calls = Arc::new(AtomicUsize::new(0));
    let client = CellClient::new(
        registry,
        Arc::new(RefusedThenUnknownTransport {
            description,
            descriptions: AtomicUsize::new(0),
            command_digest: command_digest.clone(),
            resolved_digest: resolved_digest.clone(),
            calls: calls.clone(),
        }),
    )
    .with_admission_backpressure(8, 16_384, std::time::Duration::from_secs(1))
    .unwrap();
    let now_ms = i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap();
    let identity = MutationIdentity {
        request_id: RequestId::from_bytes([7; 16]),
        issued_at_ms: now_ms,
        expires_at_ms: now_ms + 60_000,
    };

    let observed = client.clone().with_observed_description(description);
    for (index, client) in [client, observed].into_iter().enumerate() {
        let prepared = client
            .prepare_command::<PendingCommand>(&target, identity, b"input".to_vec())
            .await
            .expect("prepare exact command");
        let evidence = prepared.evidence().clone();
        let pending = match prepared.execute().await {
            Err(InvocationError::Pending(pending)) => pending,
            outcome => panic!("unexpected command outcome: {outcome:?}"),
        };
        assert_eq!(*pending, evidence);
        assert_eq!(calls.load(Ordering::SeqCst), 3 + index);
        assert_eq!(pending.identity(), identity);
        assert_eq!(
            Some(pending.operation_digest()),
            *command_digest.lock().unwrap()
        );
        assert_eq!(client.resolve(&pending).await.unwrap(), Resolution::Unknown);
        assert_eq!(
            Some(pending.operation_digest()),
            *resolved_digest.lock().unwrap()
        );
    }
}

#[tokio::test]
async fn state_stream_advances_receipts_and_cancellation_is_terminal() {
    let mut builder = RegistryBuilder::new(BuildDescriptor {
        source_revision: "stream-test".into(),
        cargo_lock_digest: Digest::from_bytes([15; 32]),
    });
    builder.register(PendingModule).unwrap();
    let registry = Arc::new(builder.finish().unwrap());
    let target = CellTarget::new(
        TenantId::from_bytes([16; 16]),
        ApplicationId::from_bytes([17; 16]),
        NAMESPACE,
        b"stream",
    )
    .unwrap();
    let description = CellDescription {
        cell: target.cell_id(),
        incarnation: IncarnationId::from_bytes([18; 16]),
        code: RETAINED_CODE,
        schema: 1,
    };
    let fenced = Arc::new(AtomicUsize::new(0));
    let client = CellClient::new(
        registry,
        Arc::new(StreamTransport {
            description,
            sequence: Arc::new(AtomicUsize::new(0)),
            fenced: fenced.clone(),
            query_started: None,
            query_release: None,
        }),
    );
    let mut stream = client
        .open_state_stream::<StreamQuery>(
            &target,
            std::time::Instant::now() + std::time::Duration::from_secs(1),
        )
        .await
        .unwrap();
    let first = stream.emit(1).await.unwrap();
    assert_eq!(first.receipt.commit_sequence, 1);
    let second = stream.emit(2).await.unwrap();
    assert_eq!(second.receipt.commit_sequence, 2);
    assert_eq!(stream.last_receipt(), Some(second.receipt));

    let cancellation = stream.cancellation();
    cancellation.cancel();
    assert!(matches!(
        stream.emit(3).await,
        Err(InvocationError::NotStarted(Error::StreamCancelled))
    ));
    assert!(stream.is_closed());

    let dropped_cancellation = {
        let dropped_stream = client
            .open_state_stream::<StreamQuery>(
                &target,
                std::time::Instant::now() + std::time::Duration::from_secs(1),
            )
            .await
            .unwrap();
        dropped_stream.cancellation()
    };
    assert!(dropped_cancellation.is_cancelled());

    assert!(matches!(
        client
            .open_state_stream::<StreamQuery>(&target, std::time::Instant::now())
            .await,
        Err(InvocationError::NotStarted(Error::Deadline))
    ));

    fenced.store(1, Ordering::Release);
    let mut fenced_stream = client
        .open_state_stream::<StreamQuery>(
            &target,
            std::time::Instant::now() + std::time::Duration::from_secs(1),
        )
        .await
        .unwrap();
    assert!(matches!(
        fenced_stream.emit(5).await,
        Err(InvocationError::NotStarted(Error::Fenced))
    ));
    assert!(fenced_stream.is_closed());

    let query_started = Arc::new(Notify::new());
    let query_release = Arc::new(Notify::new());
    let waiting_client = CellClient::new(
        client.registry.clone(),
        Arc::new(StreamTransport {
            description,
            sequence: Arc::new(AtomicUsize::new(0)),
            fenced: Arc::new(AtomicUsize::new(0)),
            query_started: Some(query_started.clone()),
            query_release: Some(query_release),
        }),
    );
    let waiting_stream = waiting_client
        .open_state_stream::<StreamQuery>(
            &target,
            std::time::Instant::now() + std::time::Duration::from_secs(1),
        )
        .await
        .unwrap();
    let cancellation = waiting_stream.cancellation();
    let waiting = tokio::spawn(async move {
        let mut waiting_stream = waiting_stream;
        waiting_stream.emit(6).await
    });
    tokio::time::timeout(std::time::Duration::from_secs(1), query_started.notified())
        .await
        .unwrap();
    cancellation.cancel();
    assert!(matches!(
        tokio::time::timeout(std::time::Duration::from_secs(1), waiting)
            .await
            .unwrap()
            .unwrap(),
        Err(InvocationError::NotStarted(Error::StreamCancelled))
    ));
}

fn admission_transport(
    requests: usize,
    bytes: usize,
    wait: std::time::Duration,
) -> super::backpressure::BackpressureTransport {
    let target = CellTarget::new(
        TenantId::from_bytes([1; 16]),
        ApplicationId::from_bytes([2; 16]),
        NAMESPACE,
        b"admission",
    )
    .unwrap();
    super::backpressure::BackpressureTransport::new(
        Arc::new(StreamTransport {
            description: CellDescription {
                cell: target.cell_id(),
                incarnation: IncarnationId::from_bytes([3; 16]),
                code: RETAINED_CODE,
                schema: 1,
            },
            sequence: Arc::default(),
            fenced: Arc::default(),
            query_started: None,
            query_release: None,
        }),
        requests,
        bytes,
        wait,
    )
    .unwrap()
}

#[tokio::test]
async fn admission_backpressure_retries_capacity_but_not_fencing() {
    let transport = admission_transport(2, 8_192, std::time::Duration::from_secs(1));
    let mut attempts = 0;
    let value = transport
        .invoke(ADMISSION_CELL, 32, Some(1), || {
            attempts += 1;
            std::future::ready(if attempts < 3 {
                Err(Error::Capacity("owner mailbox"))
            } else {
                Ok(42)
            })
        })
        .await
        .unwrap();
    assert_eq!((value, attempts), (42, 3));
    let mut attempts = 0;
    let result = transport
        .invoke(ADMISSION_CELL, 32, Some(1), || {
            attempts += 1;
            std::future::ready(Err::<(), _>(Error::Fenced))
        })
        .await;
    assert!(matches!(result, Err(Error::Fenced)) && attempts == 1);
}

#[tokio::test]
async fn admission_backpressure_releases_shared_limits_after_cancellation() {
    for (requests, bytes, resource) in [
        (1, 8_192, "client admission requests"),
        (2, 2_048, "client admission bytes"),
    ] {
        let transport = admission_transport(requests, bytes, std::time::Duration::from_secs(1));
        let clone = transport.clone();
        let entered = Notify::new();
        let mut held = Box::pin(transport.invoke(ADMISSION_CELL, 0, Some(1), || {
            entered.notify_one();
            std::future::pending::<crate::Result<()>>()
        }));
        tokio::select! {
            result = &mut held => panic!("unexpected completion: {result:?}"),
            () = entered.notified() => {}
        }
        let result = clone
            .invoke(ADMISSION_CELL, 0, Some(1), || std::future::ready(Ok(())))
            .await;
        assert!(matches!(result, Err(Error::Capacity(found)) if found == resource));
        drop(held);
        clone
            .invoke(ADMISSION_CELL, 0, Some(1), || std::future::ready(Ok(())))
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn admission_backpressure_expires_without_canceling_accepted_work() {
    let transport = admission_transport(1, 8_192, std::time::Duration::from_millis(25));
    let mut attempts = 0;
    let result = transport
        .invoke(ADMISSION_CELL, 0, Some(1), || {
            attempts += 1;
            std::future::ready(Err::<(), _>(Error::Capacity("owner mailbox")))
        })
        .await;
    assert!(matches!(result, Err(Error::Capacity("owner mailbox"))) && attempts <= 2);
    let result = transport
        .invoke(ADMISSION_CELL, 0, Some(1), || async {
            tokio::time::sleep(std::time::Duration::from_millis(40)).await;
            Ok(42)
        })
        .await
        .unwrap();
    assert_eq!(result, 42);
}

#[tokio::test]
async fn admission_backpressure_is_fifo_per_cell_and_independent_across_cells() {
    let transport = admission_transport(4, 16_384, std::time::Duration::from_secs(1));
    let mut held = Box::pin(transport.invoke(
        ADMISSION_CELL,
        0,
        Some(crate::cell::actor::CELL_BYTES),
        std::future::pending::<crate::Result<()>>,
    ));
    assert!(futures_util::poll!(&mut held).is_pending());
    let order = AtomicUsize::new(0);
    let mut queued = Box::pin(transport.invoke(
        ADMISSION_CELL,
        0,
        Some(crate::cell::actor::CELL_BYTES),
        || std::future::ready(Ok(order.fetch_add(1, Ordering::SeqCst))),
    ));
    assert!(futures_util::poll!(&mut queued).is_pending());
    transport
        .invoke(crate::CellId::from_bytes([2; 32]), 0, Some(1), || {
            std::future::ready(Ok(()))
        })
        .await
        .unwrap();
    drop(held);
    let arrival = transport.invoke(
        ADMISSION_CELL,
        0,
        Some(crate::cell::actor::CELL_BYTES),
        || std::future::ready(Ok(order.fetch_add(1, Ordering::SeqCst))),
    );
    // Poll the new arrival first: the already queued call must still run first.
    let (arrival, queued) = tokio::join!(biased; arrival, queued);
    assert_eq!((queued.unwrap(), arrival.unwrap()), (0, 1));
}

#[tokio::test]
async fn admission_backpressure_overlaps_calls_without_overtaking_large_waiters() {
    let transport = admission_transport(4, 16_384, std::time::Duration::from_secs(1));
    let half = crate::cell::actor::CELL_BYTES / 2;
    let mut held = Box::pin(transport.invoke(ADMISSION_CELL, 0, Some(half), || {
        std::future::pending::<crate::Result<()>>()
    }));
    assert!(futures_util::poll!(&mut held).is_pending());
    transport
        .invoke(ADMISSION_CELL, 0, Some(half), || std::future::ready(Ok(())))
        .await
        .unwrap();
    let mut large = Box::pin(transport.invoke(ADMISSION_CELL, 0, Some(half + 1), || {
        std::future::ready(Ok(()))
    }));
    assert!(futures_util::poll!(&mut large).is_pending());
    let mut small =
        Box::pin(transport.invoke(ADMISSION_CELL, 0, Some(1), || std::future::ready(Ok(()))));
    assert!(futures_util::poll!(&mut small).is_pending());
    // Canceling the older large waiter releases its claim on remaining bytes.
    drop(large);
    small.await.unwrap();
}

#[tokio::test]
async fn describe_admission_does_not_wait_for_the_owner_mailbox() {
    let transport = admission_transport(2, 8_192, std::time::Duration::from_millis(25));
    let target = CellTarget::new(
        TenantId::from_bytes([1; 16]),
        ApplicationId::from_bytes([2; 16]),
        NAMESPACE,
        b"admission",
    )
    .unwrap();
    let mut held = Box::pin(transport.invoke(
        target.cell_id(),
        0,
        Some(crate::cell::actor::CELL_BYTES),
        std::future::pending::<crate::Result<()>>,
    ));
    assert!(futures_util::poll!(&mut held).is_pending());
    assert_eq!(
        transport.describe(target.clone()).await.unwrap().cell,
        target.cell_id()
    );
}
