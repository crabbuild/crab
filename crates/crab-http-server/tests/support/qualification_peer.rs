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
    CellClient, CellHandle, CellId, CellTarget, Digest, Error, PeerAuthorizer, PeerCellResolver,
    PeerDispatcher, PeerPrincipal, PeerRoundTrip, PeerSigner, PeerVerifier, Registry, Result,
    SessionId, VerifiedPeerRequest,
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
    Drop {
        before_dispatch: bool,
        ordinal: usize,
        dropped: Arc<AtomicUsize>,
    },
    Pause {
        after_dispatch: bool,
        entered: Arc<Notify>,
    },
}

impl MutationFault {
    fn ordinal(&self) -> usize {
        match self {
            Self::Drop { ordinal, .. } => *ordinal,
            Self::Pause { .. } => 1,
        }
    }
}

struct FaultyMutationRoundTrip {
    verifier: Arc<PeerVerifier>,
    dispatcher: Arc<PeerDispatcher>,
    dispatched: Arc<AtomicUsize>,
    attempted: Arc<AtomicUsize>,
    fault: MutationFault,
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
        Box::pin(async move {
            let now_ms = i64::try_from(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_err(|_| Error::Control("qualification peer clock failed"))?
                    .as_millis(),
            )
            .map_err(|_| Error::Control("qualification peer clock overflow"))?;
            let verified = verifier.verify(&request, now_ms)?;
            if verified.target() != &target {
                return Err(Error::Peer("qualification peer target differs"));
            }
            let mutation = verified.operation_tag() == 10;
            let selected =
                mutation && attempted.fetch_add(1, Ordering::AcqRel) + 1 == fault.ordinal();
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
                    _ => {}
                }
            }
            let reply = dispatcher.dispatch_bytes(&verified, now_ms).await?;
            if mutation {
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
    };
    CellClient::peer(
        registry,
        signer,
        PeerPrincipal {
            issuer: "urn:crab:qualification".into(),
            subject: "qualification".into(),
            actions: vec!["cell.read".into(), "cell.write".into()],
        },
        Arc::new(round_trip),
    )
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
