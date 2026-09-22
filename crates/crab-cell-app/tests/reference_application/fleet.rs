use super::performance::now_ms;
use super::*;
use std::{collections::HashMap, net::SocketAddr, time::Duration};

use crab_cell_runtime::{
    CellId, MAX_PEER_REQUEST_BYTES, PeerAuthorizer, PeerCellResolver, PeerDispatcher,
    PeerRoundTrip, PeerVerifier, VerifiedPeerRequest,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

struct FleetResolver(Arc<HashMap<CellId, CellHandle>>);

impl PeerCellResolver for FleetResolver {
    fn resolve(
        &self,
        target: CellTarget,
    ) -> Pin<Box<dyn Future<Output = Result<CellHandle>> + Send + 'static>> {
        let handles = Arc::clone(&self.0);
        Box::pin(async move {
            handles
                .get(&target.cell_id())
                .cloned()
                .ok_or(Error::CellNotActive)
        })
    }
}

struct FleetAuthorizer;

impl PeerAuthorizer for FleetAuthorizer {
    fn authorize(&self, request: &VerifiedPeerRequest) -> Result<()> {
        let action = match request.operation_tag() {
            10 | 12 => "cell.write",
            11 => "cell.read",
            13 | 14 => "reference.cron.deliver",
            _ => return Err(Error::PeerAuthorization("unsupported fleet operation")),
        };
        if request.permits(action) {
            Ok(())
        } else {
            Err(Error::PeerAuthorization("fleet action is missing"))
        }
    }
}

struct TcpRoundTrip(Arc<HashMap<CellId, SocketAddr>>);

impl PeerRoundTrip for TcpRoundTrip {
    fn send(
        &self,
        target: CellTarget,
        request: Vec<u8>,
        remaining_ms: u32,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>>> + Send + 'static>> {
        let address = self.0.get(&target.cell_id()).copied();
        Box::pin(async move {
            let address = address.ok_or(Error::CellNotActive)?;
            let reply =
                tokio::time::timeout(Duration::from_millis(u64::from(remaining_ms)), async {
                    let mut socket = TcpStream::connect(address).await.map_err(|source| {
                        Error::PeerTransport {
                            context: "fleet peer connect",
                            source: Box::new(source),
                        }
                    })?;
                    socket
                        .write_all(&(request.len() as u32).to_be_bytes())
                        .await
                        .map_err(peer_io)?;
                    socket.write_all(&request).await.map_err(peer_io)?;
                    let mut length = [0; 4];
                    socket.read_exact(&mut length).await.map_err(peer_io)?;
                    let length = u32::from_be_bytes(length) as usize;
                    if length > MAX_PEER_REQUEST_BYTES {
                        return Err(Error::Peer("fleet reply exceeds peer byte limit"));
                    }
                    let mut reply = vec![0; length];
                    socket.read_exact(&mut reply).await.map_err(peer_io)?;
                    Ok(reply)
                })
                .await
                .map_err(|source| Error::PeerTransportUnknown {
                    context: "fleet peer deadline",
                    source: Box::new(source),
                })??;
            Ok(reply)
        })
    }
}

fn peer_io(source: std::io::Error) -> Error {
    Error::PeerTransportUnknown {
        context: "fleet peer round trip",
        source: Box::new(source),
    }
}

pub(super) async fn start_peer_servers(
    registry: &Arc<Registry>,
    verifier: Arc<PeerVerifier>,
    owned: Vec<Vec<CellHandle>>,
) -> (Arc<dyn PeerRoundTrip>, Vec<tokio::task::JoinHandle<()>>) {
    let mut endpoints = HashMap::new();
    let mut servers = Vec::new();
    for handles in owned {
        let dispatcher = Arc::new(PeerDispatcher::new(
            Arc::clone(registry),
            Arc::new(FleetResolver(Arc::new(
                handles
                    .iter()
                    .cloned()
                    .map(|handle| (handle.cell_id(), handle))
                    .collect(),
            ))),
            Arc::new(FleetAuthorizer),
        ));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        // Pin routing for this run; the receiving resolver rejects a target
        // it does not own, so a wrong route cannot pass the action checks.
        for handle in handles {
            endpoints.insert(handle.cell_id(), address);
        }
        let verifier = Arc::clone(&verifier);
        servers.push(tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                let verifier = Arc::clone(&verifier);
                let dispatcher = Arc::clone(&dispatcher);
                tokio::spawn(async move {
                    let _ = serve_peer(socket, verifier, dispatcher).await;
                });
            }
        }));
    }
    (Arc::new(TcpRoundTrip(Arc::new(endpoints))), servers)
}

async fn serve_peer(
    mut socket: TcpStream,
    verifier: Arc<PeerVerifier>,
    dispatcher: Arc<PeerDispatcher>,
) -> Result<()> {
    let mut length = [0; 4];
    socket.read_exact(&mut length).await.map_err(peer_io)?;
    let length = u32::from_be_bytes(length) as usize;
    if length > MAX_PEER_REQUEST_BYTES {
        return Err(Error::Peer("fleet request exceeds peer byte limit"));
    }
    let mut request = vec![0; length];
    socket.read_exact(&mut request).await.map_err(peer_io)?;
    let now = now_ms();
    let verified = verifier.verify(&request, now)?;
    let reply = dispatcher.dispatch_bytes(&verified, now).await?;
    socket
        .write_all(&(reply.len() as u32).to_be_bytes())
        .await
        .map_err(peer_io)?;
    socket.write_all(&reply).await.map_err(peer_io)?;
    Ok(())
}
