use std::{convert::Infallible, net::IpAddr, time::Duration};

use bytes::Bytes;
use http::{Method, Request, Response, StatusCode, header};
use http_body_util::Full;
use hyper::{body::Incoming, service::service_fn};
use hyper_util::{
    rt::{TokioExecutor, TokioIo},
    server::{conn::auto::Builder as ConnectionBuilder, graceful::GracefulShutdown},
};
use s3s::{host::SingleDomain, service::S3ServiceBuilder};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use crate::{Config, Result, gateway::Gateway};

/// Serve the configured gateway until SIGINT or SIGTERM.
pub async fn serve(config: Config) -> Result<()> {
    config.validate()?;
    let endpoint_domain = config.endpoint_domain.clone();
    let listener = TcpListener::bind(config.listen).await?;
    let management_listener = TcpListener::bind(config.management_listen).await?;
    let cancellation = CancellationToken::new();
    let gateway = Gateway::new(config, cancellation.clone())?;
    let multipart_maintenance = gateway.start_multipart_maintenance();
    let mut builder = S3ServiceBuilder::new(gateway.clone());
    builder.set_auth(gateway.auth());
    if let Some(domain) = endpoint_domain {
        builder.set_host(SingleDomain::new(&domain)?);
    }
    let service = builder.build();
    let connections = ConnectionBuilder::new(TokioExecutor::new());
    let graceful = GracefulShutdown::new();
    tracing::info!(address = %listener.local_addr()?, "Crab S3 gateway listening");
    tracing::info!(address = %management_listener.local_addr()?, "Crab S3 gateway management listener ready");
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (socket, peer) = accepted?;
                let connection = connections.serve_connection(TokioIo::new(socket), service.clone());
                let connection = graceful.watch(connection.into_owned());
                tokio::spawn(async move {
                    if let Err(error) = connection.await {
                        tracing::warn!(%peer, %error, "S3 connection failed");
                    }
                });
            }
            accepted = management_listener.accept() => {
                let (socket, peer) = accepted?;
                let gateway = gateway.clone();
                let service = service_fn(move |request| management_response(gateway.clone(), request));
                let connection = connections.serve_connection(TokioIo::new(socket), service);
                let connection = graceful.watch(connection.into_owned());
                tokio::spawn(async move {
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
    Ok(())
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
        (_, "/livez" | "/readyz") => {
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
}
