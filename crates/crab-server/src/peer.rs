use std::{io, net::SocketAddr, time::Duration};

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use axum::{
    Router,
    body::Body,
    extract::{Request, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use hyper_util::rt::TokioIo;
use tokio_util::{
    sync::CancellationToken,
    task::{AbortOnDropHandle, TaskTracker},
};

use crate::{Error, Result};

pub(crate) struct PeerTransport {
    pub http: reqwest::Client,
    pub stream_http: reqwest::Client,
    task: Option<AbortOnDropHandle<std::io::Result<()>>>,
    work: TaskTracker,
    cancellation: CancellationToken,
}

#[derive(Clone)]
struct Forwarder {
    client: reqwest::Client,
    destination: SocketAddr,
    work: TaskTracker,
    cancellation: CancellationToken,
}

impl PeerTransport {
    pub(crate) async fn start(tls: &crate::peer_tls::LoadedPeerTls) -> Result<Self> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let proxy = format!("socks5h://{}", listener.local_addr()?);
        let http = proxy_client(&proxy, Some(std::time::Duration::from_secs(30)))?;
        let stream_http = proxy_client(&proxy, None)?;
        let cancellation = CancellationToken::new();
        let stopped = cancellation.clone();
        let work = TaskTracker::new();
        let identity = tls.client_identity();
        let active = work.clone();
        let task = tokio::spawn(async move {
            loop {
                let (stream, _) = tokio::select! {
                    () = stopped.cancelled() => return Ok(()),
                    accepted = listener.accept() => accepted?,
                };
                let identity = identity.clone();
                let stopped = stopped.clone();
                active.spawn(async move {
                    tokio::select! {
                        () = stopped.cancelled() => {},
                        result = bridge(stream, identity) => {
                            if let Err(error) = result {
                                tracing::debug!(error = %error, "Cellar peer connection closed");
                            }
                        }
                    }
                });
            }
        });
        Ok(Self {
            http,
            stream_http,
            task: Some(AbortOnDropHandle::new(task)),
            work,
            cancellation,
        })
    }

    pub(crate) async fn shutdown(mut self) -> Result<()> {
        self.cancellation.cancel();
        let stopped = match self.task.take() {
            Some(task) => Some(task.await),
            None => None,
        };
        self.work.close();
        self.work.wait().await;
        if let Some(result) = stopped {
            result??;
        }
        Ok(())
    }
}

impl Drop for PeerTransport {
    fn drop(&mut self) {
        self.cancellation.cancel();
        self.work.close();
    }
}

fn proxy_client(proxy: &str, timeout: Option<std::time::Duration>) -> Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder()
        .no_proxy()
        .proxy(reqwest::Proxy::all(proxy).map_err(|source| Error::PeerTls {
            context: "Cellar transport proxy is invalid",
            source: Box::new(source),
        })?)
        .connect_timeout(std::time::Duration::from_secs(5))
        .redirect(reqwest::redirect::Policy::none());
    if let Some(timeout) = timeout {
        builder = builder.timeout(timeout);
    }
    builder.build().map_err(|source| Error::PeerTls {
        context: "Cellar transport client initialization failed",
        source: Box::new(source),
    })
}

// Complete TLS before reporting SOCKS success. Reqwest then classifies a
// refused peer as a connect failure, preserving Cellar's safe redispatch.
// An HTTP forwarding proxy would turn it into an ambiguous response instead.
async fn bridge(
    mut stream: tokio::net::TcpStream,
    identity: crate::peer_tls::PeerTlsClient,
) -> io::Result<()> {
    let connect = async {
        let version = stream.read_u8().await?;
        let count = stream.read_u8().await?;
        let mut methods = vec![0; usize::from(count)];
        stream.read_exact(&mut methods).await?;
        if version != 5 || !methods.contains(&0) {
            stream.write_all(&[5, 255]).await?;
            return Err(io::Error::other("unsupported peer proxy authentication"));
        }
        stream.write_all(&[5, 0]).await?;
        let mut request = [0; 4];
        stream.read_exact(&mut request).await?;
        if request[..3] != [5, 1, 0] {
            return Err(io::Error::other("unsupported peer proxy command"));
        }
        let host = match request[3] {
            1 => {
                let mut address = [0; 4];
                stream.read_exact(&mut address).await?;
                std::net::Ipv4Addr::from(address).to_string()
            }
            4 => {
                let mut address = [0; 16];
                stream.read_exact(&mut address).await?;
                std::net::Ipv6Addr::from(address).to_string()
            }
            3 => {
                let length = stream.read_u8().await?;
                let mut address = vec![0; usize::from(length)];
                stream.read_exact(&mut address).await?;
                String::from_utf8(address)
                    .map_err(|source| io::Error::new(io::ErrorKind::InvalidData, source))?
            }
            _ => return Err(io::Error::other("unsupported peer proxy address")),
        };
        let port = stream.read_u16().await?;
        match identity.connect(&host, port).await {
            Ok(peer) => {
                stream.write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0]).await?;
                Ok(peer)
            }
            Err(error) => {
                stream.write_all(&[5, 1, 0, 1, 0, 0, 0, 0, 0, 0]).await?;
                Err(error)
            }
        }
    };
    let mut peer = tokio::time::timeout(Duration::from_secs(5), connect).await??;
    tokio::io::copy_bidirectional(&mut stream, &mut peer).await?;
    Ok(())
}

pub(crate) fn incoming(
    address: SocketAddr,
    work: TaskTracker,
    cancellation: CancellationToken,
) -> Result<Router> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|source| Error::PeerTls {
            context: "Cellar local transport initialization failed",
            source: Box::new(source),
        })?;
    Ok(Router::new().fallback(forward).with_state(Forwarder {
        client,
        destination: address,
        work,
        cancellation,
    }))
}

async fn forward(State(state): State<Forwarder>, mut request: Request) -> Response {
    let destination = format!(
        "http://{}{}",
        state.destination,
        request
            .uri()
            .path_and_query()
            .map_or("/", |path| path.as_str())
    );
    let upgrading = request.headers().get(header::UPGRADE).is_some();
    let upgrade = upgrading.then(|| hyper::upgrade::on(&mut request));
    let (parts, body) = request.into_parts();
    let mut headers = parts.headers;
    headers.remove(header::HOST);
    let builder = state
        .client
        .request(parts.method, destination)
        .headers(headers);
    let builder = if http_body::Body::is_end_stream(&body) {
        builder
    } else {
        builder.body(reqwest::Body::wrap_stream(body.into_data_stream()))
    };
    let result = builder.send().await;
    let response = match result {
        Ok(response) => response,
        Err(error) => {
            tracing::warn!(error = %error, "Cellar peer transport failed");
            return StatusCode::BAD_GATEWAY.into_response();
        }
    };
    let status = response.status();
    let headers = response.headers().clone();
    let body = if status == StatusCode::SWITCHING_PROTOCOLS {
        let Some(upgrade) = upgrade else {
            return StatusCode::BAD_GATEWAY.into_response();
        };
        state.work.spawn(async move {
            let tunnel = async move {
                match (upgrade.await, response.upgrade().await) {
                    (Ok(incoming), Ok(mut outgoing)) => {
                        let mut incoming = TokioIo::new(incoming);
                        if let Err(error) =
                            tokio::io::copy_bidirectional(&mut incoming, &mut outgoing).await
                        {
                            tracing::debug!(error = %error, "Cellar peer tunnel closed");
                        }
                    }
                    _ => tracing::warn!("Cellar peer tunnel upgrade failed"),
                }
            };
            tokio::select! { () = state.cancellation.cancelled() => {}, () = tunnel => {} }
        });
        Body::empty()
    } else {
        Body::from_stream(response.bytes_stream())
    };
    let mut response = Response::new(body);
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn refused_tls_peer_remains_a_connect_error_for_native_redispatch() {
        let files = crate::peer_tls::tests::IdentityFiles::generate();
        let config = files.config(url::Url::parse("https://localhost:8789").unwrap());
        let tls = crate::peer_tls::LoadedPeerTls::load(&config).unwrap();
        let transport = PeerTransport::start(&tls).await.unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let error = transport
            .http
            .get(format!("http://{address}/peer/tunnel"))
            .send()
            .await
            .unwrap_err();
        assert!(error.is_connect(), "{error:?}");
        transport.shutdown().await.unwrap();
    }
}
