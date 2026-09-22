use std::{
    collections::HashMap,
    future::Future,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use crab_cell_runtime::{
    CellClient, CellHandle, CellId, CellTarget, Digest, EffectPeerClient, Error, PeerAuthorizer,
    PeerCellResolver, PeerDispatcher, PeerPrincipal, PeerRoundTrip, PeerSigner, PeerVerifier,
    Registry, Result, SessionId, VerifiedPeerRequest,
};
use ed25519_dalek::SigningKey;
use tokio::sync::Notify;

struct QualificationResolver(Arc<HashMap<CellId, CellHandle>>);

impl PeerCellResolver for QualificationResolver {
    fn resolve(
        &self,
        target: CellTarget,
    ) -> Pin<Box<dyn Future<Output = Result<CellHandle>> + Send + 'static>> {
        let handles = Arc::clone(&self.0);
        Box::pin(async move {
            handles
                .get(&target.cell_id())
                .cloned()
                .ok_or(Error::Identity("qualification peer Cell is missing"))
        })
    }
}

struct QualificationAuthorizer;

impl PeerAuthorizer for QualificationAuthorizer {
    fn authorize(&self, request: &VerifiedPeerRequest) -> Result<()> {
        if request.principal().subject != "qualification" {
            return Err(Error::Peer("qualification peer principal differs"));
        }
        Ok(())
    }
}

#[derive(Clone)]
enum MutationFault {
    None,
    Drop {
        before_dispatch: bool,
        ordinal: usize,
        dropped: Arc<AtomicUsize>,
    },
    Pause {
        after_dispatch: bool,
        entered: Arc<Notify>,
    },
    DelayReceive {
        entered: Arc<Notify>,
        release: Arc<Notify>,
    },
}

impl MutationFault {
    fn ordinal(&self) -> usize {
        match self {
            Self::None => usize::MAX,
            Self::Drop { ordinal, .. } => *ordinal,
            Self::Pause { .. } | Self::DelayReceive { .. } => 1,
        }
    }
}

struct FaultyMutationRoundTrip {
    verifier: Arc<PeerVerifier>,
    dispatcher: Arc<PeerDispatcher>,
    dispatched: Arc<AtomicUsize>,
    attempted: Arc<AtomicUsize>,
    fault: MutationFault,
    operation_tag: u32,
}

impl PeerRoundTrip for FaultyMutationRoundTrip {
    fn send(
        &self,
        target: CellTarget,
        request: Vec<u8>,
        _remaining_ms: u32,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>>> + Send + 'static>> {
        let verifier = Arc::clone(&self.verifier);
        let dispatcher = Arc::clone(&self.dispatcher);
        let dispatched = Arc::clone(&self.dispatched);
        let attempted = Arc::clone(&self.attempted);
        let fault = self.fault.clone();
        let operation_tag = self.operation_tag;
        Box::pin(async move {
            let mut now_ms = i64::try_from(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_err(|_| Error::Control("qualification peer clock failed"))?
                    .as_millis(),
            )
            .map_err(|_| Error::Control("qualification peer clock overflow"))?;
            let mut verified = verifier.verify(&request, now_ms)?;
            if verified.target() != &target {
                return Err(Error::Peer("qualification peer target differs"));
            }
            let selected_operation = verified.operation_tag() == operation_tag;
            let selected = selected_operation
                && attempted.fetch_add(1, Ordering::AcqRel) + 1 == fault.ordinal();
            if selected {
                match &fault {
                    MutationFault::Drop {
                        before_dispatch: true,
                        dropped,
                        ..
                    } => {
                        dropped.store(1, Ordering::Release);
                        return Err(Error::PeerTransportUnknown {
                            context: "qualification request lost before dispatch",
                            source: Box::new(Error::RuntimeClosed),
                        });
                    }
                    MutationFault::Pause {
                        after_dispatch: false,
                        entered,
                    } => {
                        entered.notify_one();
                        std::future::pending::<()>().await;
                    }
                    MutationFault::DelayReceive { entered, release } => {
                        entered.notify_one();
                        release.notified().await;
                        // Recheck the signed bytes at arrival time so a delayed
                        // delivery cannot reuse the pre-delay validation clock.
                        now_ms = i64::try_from(
                            SystemTime::now()
                                .duration_since(UNIX_EPOCH)
                                .map_err(|_| Error::Control("qualification peer clock failed"))?
                                .as_millis(),
                        )
                        .map_err(|_| Error::Control("qualification peer clock overflow"))?;
                        verified = verifier.verify(&request, now_ms)?;
                    }
                    _ => {}
                }
            }
            let reply = dispatcher.dispatch_bytes(&verified, now_ms).await?;
            if selected_operation {
                dispatched.fetch_add(1, Ordering::AcqRel);
            }
            if selected {
                match &fault {
                    MutationFault::Drop {
                        before_dispatch: false,
                        dropped,
                        ..
                    } => {
                        dropped.store(1, Ordering::Release);
                        return Err(Error::PeerTransportUnknown {
                            context: "qualification response lost after dispatch",
                            source: Box::new(Error::RuntimeClosed),
                        });
                    }
                    MutationFault::Pause {
                        after_dispatch: true,
                        entered,
                    } => {
                        entered.notify_one();
                        std::future::pending::<()>().await;
                    }
                    _ => {}
                }
            }
            Ok(reply)
        })
    }
}

fn peer_client_with_fault(
    registry: Arc<Registry>,
    handles: Vec<CellHandle>,
    fault: MutationFault,
    dispatched: Arc<AtomicUsize>,
) -> CellClient {
    let (signer, round_trip) =
        peer_transport_with_fault(registry.clone(), handles, fault, dispatched, 10);
    CellClient::peer(registry, signer, qualification_principal(), round_trip)
}

fn peer_transport_with_fault(
    registry: Arc<Registry>,
    handles: Vec<CellHandle>,
    fault: MutationFault,
    dispatched: Arc<AtomicUsize>,
    operation_tag: u32,
) -> (Arc<PeerSigner>, Arc<dyn PeerRoundTrip>) {
    let session = SessionId::from_bytes([96; 16]);
    let release = Digest::from_bytes([97; 32]);
    let signer = Arc::new(PeerSigner::new(
        session,
        release,
        SigningKey::from_bytes(&[98; 32]),
    ));
    let verifier = PeerVerifier::new(session, release, signer.verifying_key());
    let resolver = QualificationResolver(Arc::new(
        handles
            .into_iter()
            .map(|handle| (handle.cell_id(), handle))
            .collect(),
    ));
    let round_trip = FaultyMutationRoundTrip {
        verifier: Arc::new(verifier),
        dispatcher: Arc::new(PeerDispatcher::new(
            Arc::clone(&registry),
            Arc::new(resolver),
            Arc::new(QualificationAuthorizer),
        )),
        dispatched,
        attempted: Arc::new(AtomicUsize::new(0)),
        fault,
        operation_tag,
    };
    (signer, Arc::new(round_trip))
}

fn qualification_principal() -> PeerPrincipal {
    PeerPrincipal {
        issuer: "urn:crab:qualification".into(),
        subject: "qualification".into(),
        actions: vec!["cell.read".into(), "cell.write".into()],
    }
}

pub fn peer_client_with_one_lost_mutation(
    registry: Arc<Registry>,
    handles: Vec<CellHandle>,
    drop_before_dispatch: bool,
    fault_ordinal: usize,
) -> (CellClient, Arc<AtomicUsize>, Arc<AtomicUsize>) {
    assert!(fault_ordinal > 0, "fault ordinal must name a mutation");
    let dropped = Arc::new(AtomicUsize::new(0));
    let dispatched = Arc::new(AtomicUsize::new(0));
    let client = peer_client_with_fault(
        registry,
        handles,
        MutationFault::Drop {
            before_dispatch: drop_before_dispatch,
            ordinal: fault_ordinal,
            dropped: Arc::clone(&dropped),
        },
        Arc::clone(&dispatched),
    );
    (client, dropped, dispatched)
}

pub fn peer_client_with_paused_mutation(
    registry: Arc<Registry>,
    handles: Vec<CellHandle>,
    pause_after_dispatch: bool,
) -> (CellClient, Arc<Notify>, Arc<AtomicUsize>) {
    let entered = Arc::new(Notify::new());
    let dispatched = Arc::new(AtomicUsize::new(0));
    let client = peer_client_with_fault(
        registry,
        handles,
        MutationFault::Pause {
            after_dispatch: pause_after_dispatch,
            entered: Arc::clone(&entered),
        },
        Arc::clone(&dispatched),
    );
    (client, entered, dispatched)
}

pub fn peer_client_with_delayed_mutation_receive(
    registry: Arc<Registry>,
    handles: Vec<CellHandle>,
) -> (CellClient, Arc<Notify>, Arc<Notify>, Arc<AtomicUsize>) {
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let dispatched = Arc::new(AtomicUsize::new(0));
    let client = peer_client_with_fault(
        registry,
        handles,
        MutationFault::DelayReceive {
            entered: Arc::clone(&entered),
            release: Arc::clone(&release),
        },
        Arc::clone(&dispatched),
    );
    (client, entered, release, dispatched)
}

pub fn peer_effect_client_with_paused_delivery(
    registry: Arc<Registry>,
    handles: Vec<CellHandle>,
    pause_after_dispatch: bool,
) -> (EffectPeerClient, Arc<Notify>, Arc<AtomicUsize>) {
    let entered = Arc::new(Notify::new());
    let dispatched = Arc::new(AtomicUsize::new(0));
    let (signer, round_trip) = peer_transport_with_fault(
        registry,
        handles,
        MutationFault::Pause {
            after_dispatch: pause_after_dispatch,
            entered: Arc::clone(&entered),
        },
        Arc::clone(&dispatched),
        13,
    );
    (
        EffectPeerClient::new(signer, qualification_principal(), round_trip),
        entered,
        dispatched,
    )
}

pub fn peer_effect_client_with_delayed_receive(
    registry: Arc<Registry>,
    handles: Vec<CellHandle>,
) -> (EffectPeerClient, Arc<Notify>, Arc<Notify>, Arc<AtomicUsize>) {
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let dispatched = Arc::new(AtomicUsize::new(0));
    let (signer, round_trip) = peer_transport_with_fault(
        registry,
        handles,
        MutationFault::DelayReceive {
            entered: Arc::clone(&entered),
            release: Arc::clone(&release),
        },
        Arc::clone(&dispatched),
        13,
    );
    (
        EffectPeerClient::new(signer, qualification_principal(), round_trip),
        entered,
        release,
        dispatched,
    )
}

pub fn peer_effect_client(registry: Arc<Registry>, handles: Vec<CellHandle>) -> EffectPeerClient {
    let (signer, round_trip) = peer_transport_with_fault(
        registry,
        handles,
        MutationFault::None,
        Arc::new(AtomicUsize::new(0)),
        13,
    );
    EffectPeerClient::new(signer, qualification_principal(), round_trip)
}
