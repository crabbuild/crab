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

struct FaultyMutationRoundTrip {
    verifier: Arc<PeerVerifier>,
    dispatcher: Arc<PeerDispatcher>,
    dropped: Arc<AtomicUsize>,
    dispatched: Arc<AtomicUsize>,
    drop_before_dispatch: bool,
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
        let dropped = Arc::clone(&self.dropped);
        let dispatched = Arc::clone(&self.dispatched);
        let drop_before_dispatch = self.drop_before_dispatch;
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
            let first_loss = || {
                mutation
                    && dropped
                        .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
                        .is_ok()
            };
            if drop_before_dispatch && first_loss() {
                return Err(Error::PeerTransportUnknown {
                    context: "qualification request lost before dispatch",
                    source: Box::new(Error::RuntimeClosed),
                });
            }
            let reply = dispatcher.dispatch_bytes(&verified, now_ms).await?;
            if mutation {
                dispatched.fetch_add(1, Ordering::AcqRel);
            }
            if !drop_before_dispatch && first_loss() {
                return Err(Error::PeerTransportUnknown {
                    context: "qualification response lost after dispatch",
                    source: Box::new(Error::RuntimeClosed),
                });
            }
            Ok(reply)
        })
    }
}

pub fn peer_client_with_one_lost_mutation(
    registry: Arc<Registry>,
    handles: Vec<CellHandle>,
    drop_before_dispatch: bool,
) -> (CellClient, Arc<AtomicUsize>, Arc<AtomicUsize>) {
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
    let dropped = Arc::new(AtomicUsize::new(0));
    let dispatched = Arc::new(AtomicUsize::new(0));
    let round_trip = FaultyMutationRoundTrip {
        verifier: Arc::new(verifier),
        dispatcher: Arc::new(PeerDispatcher::new(
            Arc::clone(&registry),
            Arc::new(resolver),
            Arc::new(QualificationAuthorizer),
        )),
        dropped: Arc::clone(&dropped),
        dispatched: Arc::clone(&dispatched),
        drop_before_dispatch,
    };
    let client = CellClient::peer(
        registry,
        signer,
        PeerPrincipal {
            issuer: "urn:crab:qualification".into(),
            subject: "qualification".into(),
            actions: vec!["cell.read".into(), "cell.write".into()],
        },
        Arc::new(round_trip),
    );
    (client, dropped, dispatched)
}
