use std::{
    convert::Infallible,
    net::IpAddr,
    sync::{Arc, Mutex},
    time::Duration,
};

use bytes::Bytes;
use http::{Method, Request, Response, StatusCode, header};
use http_body::Body as _;
use http_body_util::{BodyExt as _, Full};
use hyper::{body::Incoming, service::service_fn};
use hyper_util::{
    rt::{TokioExecutor, TokioIo, TokioTimer},
    server::{conn::auto::Builder as ConnectionBuilder, graceful::GracefulShutdown},
};
use s3s::{
    host::SingleDomain,
    service::{S3Service, S3ServiceBuilder},
};
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use crate::{
    Config, Result,
    content::{BODY_IDLE_TIMEOUT, Digester},
    gateway::Gateway,
    metrics::{Metrics, ObservedBody},
};

const CONNECTIONS_PER_REQUEST: usize = 4;
const MANAGEMENT_CONNECTIONS: usize = 16;
const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_HTTP1_HEADERS: usize = 128;
const MAX_HTTP1_BUFFER_BYTES: usize = 128 * 1024;
const MAX_HTTP2_HEADER_LIST_BYTES: u32 = 128 * 1024;
const MAX_HTTP2_STREAMS_PER_CONNECTION: u32 = 64;

#[derive(Clone)]
pub(crate) struct RequestBodyDigest {
    digester: Arc<Mutex<Option<Digester>>>,
}

impl RequestBodyDigest {
    pub(crate) fn new() -> Self {
        Self {
            digester: Arc::new(Mutex::new(Some(Digester::new()))),
        }
    }

    pub(crate) fn update(&self, bytes: &[u8]) {
        if let Ok(mut digester) = self.digester.lock()
            && let Some(digester) = digester.as_mut()
        {
            let _ = digester.write(bytes, u64::MAX);
        }
    }

    pub(crate) fn finish(&self) -> Option<crate::content::Digests> {
        self.digester
            .lock()
            .ok()?
            .take()?
            .finish()
            .ok()
            .map(|(_, digests)| digests)
    }
}

/// Serve the configured gateway until SIGINT or SIGTERM.
pub async fn serve(config: Config) -> Result<()> {
    config.validate()?;
    let endpoint_domain = config.endpoint_domain.clone();
    let max_in_flight_requests = config.max_in_flight_requests;
    let listener = TcpListener::bind(config.listen).await?;
    let management_listener = TcpListener::bind(config.management_listen).await?;
    let listen_address = listener.local_addr()?;
    let management_address = management_listener.local_addr()?;
    let cancellation = CancellationToken::new();
    let gateway = Gateway::new(config, cancellation.clone())?;
    let multipart_maintenance = gateway.start_multipart_maintenance();
    let s3_connections = Arc::new(Semaphore::new(connection_capacity(max_in_flight_requests)));
    let management_connections = Arc::new(Semaphore::new(MANAGEMENT_CONNECTIONS));
    let mut builder = S3ServiceBuilder::new(gateway.clone());
    let auth = gateway.auth();
    builder.set_auth(auth.clone());
    builder.set_access(auth);
    if let Some(domain) = endpoint_domain {
        builder.set_host(SingleDomain::new(&domain)?);
    }
    let service = builder.build();
    let mut connections = ConnectionBuilder::new(TokioExecutor::new());
    connections
        .http1()
        .header_read_timeout(HEADER_READ_TIMEOUT)
        .max_headers(MAX_HTTP1_HEADERS)
        .max_buf_size(MAX_HTTP1_BUFFER_BYTES)
        .timer(TokioTimer::new());
    connections
        .http2()
        .max_concurrent_streams(
            (max_in_flight_requests as u32).min(MAX_HTTP2_STREAMS_PER_CONNECTION),
        )
        .max_header_list_size(MAX_HTTP2_HEADER_LIST_BYTES)
        .keep_alive_interval(Some(Duration::from_secs(30)))
        .keep_alive_timeout(Duration::from_secs(10))
        .timer(TokioTimer::new());
    let graceful = GracefulShutdown::new();
    tracing::info!(address = %listen_address, "Crab S3 gateway listening");
    tracing::info!(address = %management_address, "Crab S3 gateway management listener ready");
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);
    let mut accept_error = None;

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (socket, peer) = match accepted {
                    Ok(accepted) => accepted,
                    Err(error) => {
                        tracing::error!(%error, "S3 listener accept failed; draining existing connections");
                        accept_error = Some(error);
                        break;
                    }
                };
                let Ok(connection_permit) = Arc::clone(&s3_connections).try_acquire_owned() else {
                    tracing::debug!(%peer, "S3 connection capacity exhausted");
                    continue;
                };
                let s3_service = service.clone();
                let metrics = gateway.metrics();
                let service = service_fn(move |request| {
                    observed_s3_response(s3_service.clone(), metrics.clone(), request)
                });
                let connection = connections.serve_connection(TokioIo::new(socket), service);
                let connection = graceful.watch(connection.into_owned());
                tokio::spawn(async move {
                    let _connection_permit = connection_permit;
                    if let Err(error) = connection.await {
                        tracing::warn!(%peer, %error, "S3 connection failed");
                    }
                });
            }
            accepted = management_listener.accept() => {
                let (socket, peer) = match accepted {
                    Ok(accepted) => accepted,
                    Err(error) => {
                        tracing::error!(%error, "management listener accept failed; draining existing connections");
                        accept_error = Some(error);
                        break;
                    }
                };
                let Ok(connection_permit) = Arc::clone(&management_connections).try_acquire_owned() else {
                    tracing::debug!(%peer, "management connection capacity exhausted");
                    continue;
                };
                let gateway = gateway.clone();
                let service = service_fn(move |request| management_response(gateway.clone(), request));
                let connection = connections.serve_connection(TokioIo::new(socket), service);
                let connection = graceful.watch(connection.into_owned());
                tokio::spawn(async move {
                    let _connection_permit = connection_permit;
                    if let Err(error) = connection.await {
                        tracing::warn!(%peer, %error, "management connection failed");
                    }
                });
            }
            () = &mut shutdown => break,
        }
    }

    cancellation.cancel();
    tokio::select! {
        () = graceful.shutdown() => {}
        () = tokio::time::sleep(Duration::from_secs(30)) => {
            tracing::warn!("gateway connections exceeded graceful shutdown deadline");
        }
    }
    gateway.shutdown().await;
    multipart_maintenance.await?;
    if let Some(error) = accept_error {
        return Err(error.into());
    }
    Ok(())
}

fn connection_capacity(max_in_flight_requests: usize) -> usize {
    max_in_flight_requests
        .saturating_mul(CONNECTIONS_PER_REQUEST)
        .max(MANAGEMENT_CONNECTIONS)
}

/// Check the running process without accessing repository storage.
pub async fn check_liveness(config: &Config) -> Result<()> {
    check_management(config, "/livez").await
}

/// Check that every configured repository is safe to serve.
pub async fn check_readiness(config: &Config) -> Result<()> {
    check_management(config, "/readyz").await
}

async fn check_management(config: &Config, path: &str) -> Result<()> {
    let address = probe_address(config.management_listen);
    let url = format!("http://{address}{path}");
    reqwest::Client::builder()
        .timeout(Duration::from_secs(12))
        .build()
        .map_err(|source| crate::Error::Healthcheck { source })?
        .get(url)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|source| crate::Error::Healthcheck { source })?;
    Ok(())
}

fn probe_address(address: std::net::SocketAddr) -> std::net::SocketAddr {
    if !address.ip().is_unspecified() {
        return address;
    }
    let ip = match address.ip() {
        IpAddr::V4(_) => IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
        IpAddr::V6(_) => IpAddr::V6(std::net::Ipv6Addr::LOCALHOST),
    };
    std::net::SocketAddr::new(ip, address.port())
}

async fn management_response(
    gateway: Gateway,
    request: Request<Incoming>,
) -> std::result::Result<Response<Full<Bytes>>, Infallible> {
    let response = match (request.method(), request.uri().path()) {
        (&Method::GET, "/livez") => json_response(StatusCode::OK, b"{\"status\":\"ok\"}"),
        (&Method::GET, "/readyz") => readiness_response(&gateway).await,
        (&Method::GET, "/metrics") => metrics_response(gateway.render_metrics().await),
        (_, "/livez" | "/readyz" | "/metrics") => {
            let mut response = json_response(
                StatusCode::METHOD_NOT_ALLOWED,
                b"{\"status\":\"method_not_allowed\"}",
            );
            response
                .headers_mut()
                .insert(header::ALLOW, http::HeaderValue::from_static("GET"));
            response
        }
        _ => json_response(StatusCode::NOT_FOUND, b"{\"status\":\"not_found\"}"),
    };
    Ok(response)
}

async fn observed_s3_response(
    service: S3Service,
    metrics: Metrics,
    request: Request<Incoming>,
) -> std::result::Result<Response<ObservedBody>, s3s::HttpError> {
    let observation = metrics.start_request(request.method());
    let request = request_body_with_digest(request);
    match service.call(request).await {
        Ok(response) => {
            let observation = observation.response(response.status());
            let (parts, body) = response.into_parts();
            Ok(Response::from_parts(
                parts,
                ObservedBody::new(body, observation),
            ))
        }
        Err(error) => {
            observation.transport_error();
            Err(error)
        }
    }
}

fn request_body_with_digest(request: Request<Incoming>) -> Request<s3s::Body> {
    let needs_digest = needs_xml_body_digest(&request);
    let (parts, body) = request.into_parts();
    if needs_digest {
        // `s3s` buffers XML mutation requests before dispatch; bound that body at
        // the transport boundary so a client cannot hold a connection forever.
        let body = body_with_idle_timeout(body, BODY_IDLE_TIMEOUT);
        let digest = RequestBodyDigest::new();
        let digest_for_body = digest.clone();
        let body = body.map_frame(move |frame| {
            if let Some(data) = frame.data_ref() {
                digest_for_body.update(data.as_ref());
            }
            frame
        });
        let mut request = Request::from_parts(parts, s3s::Body::http_body_unsync(body));
        request.extensions_mut().insert(digest);
        request
    } else {
        Request::from_parts(parts, s3s::Body::from(body))
    }
}

fn body_with_idle_timeout<B>(
    body: B,
    idle_timeout: Duration,
) -> impl http_body::Body<Data = Bytes, Error = std::io::Error> + Send + 'static
where
    B: http_body::Body<Data = Bytes> + Send + 'static,
    B::Error: std::error::Error + Send + Sync + 'static,
{
    let body = Box::pin(http_body_util::BodyStream::new(body));
    let stream = futures_util::stream::unfold((body, false), move |(mut body, done)| async move {
        if done {
            return None;
        }
        let frame = tokio::time::timeout(
            idle_timeout,
            std::future::poll_fn(|context| body.as_mut().poll_frame(context)),
        )
        .await;
        match frame {
            Ok(Some(Ok(frame))) => Some((Ok(frame), (body, false))),
            Ok(Some(Err(error))) => Some((Err(std::io::Error::other(error)), (body, true))),
            Ok(None) => None,
            Err(_) => Some((
                Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "request body was idle for too long",
                )),
                (body, true),
            )),
        }
    });
    http_body_util::StreamBody::new(stream)
}

fn needs_xml_body_digest(request: &Request<Incoming>) -> bool {
    match *request.method() {
        Method::POST => has_query_flag(request.uri(), "delete"),
        Method::PUT => has_query_flag(request.uri(), "tagging"),
        _ => false,
    }
}

fn has_query_flag(uri: &http::Uri, flag: &str) -> bool {
    uri.query().is_some_and(|query| {
        query.split('&').any(|pair| {
            let key = pair.split_once('=').map_or(pair, |(key, _)| key);
            key == flag
        })
    })
}

async fn readiness_response(gateway: &Gateway) -> Response<Full<Bytes>> {
    match tokio::time::timeout(Duration::from_secs(10), gateway.ready()).await {
        Ok(Ok(())) => json_response(StatusCode::OK, b"{\"status\":\"ready\"}"),
        Ok(Err(error)) => {
            tracing::warn!(?error, "gateway repository readiness check failed");
            readiness_unavailable()
        }
        Err(_) => {
            tracing::warn!("gateway repository readiness check timed out");
            readiness_unavailable()
        }
    }
}

fn readiness_unavailable() -> Response<Full<Bytes>> {
    let mut response = json_response(
        StatusCode::SERVICE_UNAVAILABLE,
        b"{\"status\":\"unavailable\"}",
    );
    response
        .headers_mut()
        .insert(header::RETRY_AFTER, http::HeaderValue::from_static("5"));
    response
}

fn json_response(status: StatusCode, body: &'static [u8]) -> Response<Full<Bytes>> {
    let mut response = Response::new(Full::new(Bytes::from_static(body)));
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        http::HeaderValue::from_static("application/json"),
    );
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        http::HeaderValue::from_static("no-store"),
    );
    response
}

fn metrics_response(body: String) -> Response<Full<Bytes>> {
    let mut response = Response::new(Full::new(Bytes::from(body)));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        http::HeaderValue::from_static("text/plain; version=0.0.4; charset=utf-8"),
    );
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        http::HeaderValue::from_static("no-store"),
    );
    response
}

/// Initialize every configured repository without starting the listener.
pub async fn initialize(config: Config) -> Result<()> {
    config.validate()?;
    let gateway = Gateway::new(config, CancellationToken::new())?;
    gateway.initialize_repositories().await?;
    gateway.shutdown().await;
    Ok(())
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate());
        match terminate {
            Ok(mut terminate) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = terminate.recv() => {}
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unspecified_probe_addresses_use_matching_loopback_family() {
        assert_eq!(
            probe_address("0.0.0.0:8081".parse().unwrap()),
            "127.0.0.1:8081".parse().unwrap()
        );
        assert_eq!(
            probe_address("[::]:8081".parse().unwrap()),
            "[::1]:8081".parse().unwrap()
        );
    }

    #[test]
    fn explicit_probe_address_is_preserved() {
        let address = "192.0.2.10:8081".parse().unwrap();
        assert_eq!(probe_address(address), address);
    }

    #[test]
    fn connection_capacity_scales_without_exceeding_the_request_budget_multiplier() {
        assert_eq!(connection_capacity(8), 32);
        assert_eq!(connection_capacity(4096), 16_384);
    }

    #[test]
    fn management_responses_are_non_cacheable_json() {
        for (status, body) in [
            (StatusCode::OK, b"{\"status\":\"ok\"}".as_slice()),
            (
                StatusCode::SERVICE_UNAVAILABLE,
                b"{\"status\":\"unavailable\"}".as_slice(),
            ),
        ] {
            let response = json_response(status, body);
            assert_eq!(response.status(), status);
            assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
            assert_eq!(response.headers()[header::CONTENT_TYPE], "application/json");
        }
    }

    #[test]
    fn metrics_response_uses_prometheus_content_type_and_disables_caching() {
        let response = metrics_response("metric 1\n".to_owned());

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "text/plain; version=0.0.4; charset=utf-8"
        );
    }

    #[test]
    fn xml_body_query_flags_are_selected_without_matching_prefixes() {
        assert!(has_query_flag(&"/repo?delete".parse().unwrap(), "delete"));
        assert!(has_query_flag(
            &"/repo/key?tagging=".parse().unwrap(),
            "tagging"
        ));
        assert!(!has_query_flag(
            &"/repo?delete-marker".parse().unwrap(),
            "delete"
        ));
    }

    #[test]
    fn request_body_digest_can_only_be_finished_once() {
        use md5::Digest as _;

        let digest = RequestBodyDigest::new();
        digest.update(b"tagging body");
        let digests = digest.finish().unwrap();
        let expected: [u8; 16] = md5::Md5::digest(b"tagging body").into();
        assert_eq!(digests.md5, expected);
        assert!(digest.finish().is_none());
    }

    #[tokio::test]
    async fn request_body_idle_timeout_stops_a_stalled_stream() {
        let body = http_body_util::StreamBody::new(futures_util::stream::pending::<
            std::result::Result<http_body::Frame<Bytes>, std::io::Error>,
        >());
        let error = body_with_idle_timeout(body, Duration::from_millis(1))
            .collect()
            .await
            .unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
    }

    #[tokio::test]
    async fn request_body_idle_timeout_preserves_data_and_trailers() {
        let mut trailers = http::HeaderMap::new();
        trailers.insert("x-test-trailer", "present".parse().unwrap());
        let body = http_body_util::StreamBody::new(futures_util::stream::iter([
            Ok::<_, std::io::Error>(http_body::Frame::data(Bytes::from_static(b"body"))),
            Ok::<_, std::io::Error>(http_body::Frame::trailers(trailers.clone())),
        ]));

        let collected = body_with_idle_timeout(body, Duration::from_secs(1))
            .collect()
            .await
            .unwrap();

        assert_eq!(collected.trailers(), Some(&trailers));
        assert_eq!(collected.to_bytes(), Bytes::from_static(b"body"));
    }
}
