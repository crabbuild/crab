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

    loop {
        let accepted = tokio::select! {
            accepted = listener.accept() => Some(accepted?),
            () = shutdown_signal() => None,
        };
        let Some((socket, peer)) = accepted else {
            break;
        };
        let connection = connections.serve_connection(TokioIo::new(socket), service.clone());
        let connection = graceful.watch(connection.into_owned());
        tokio::spawn(async move {
            if let Err(error) = connection.await {
                tracing::warn!(%peer, %error, "S3 connection failed");
            }
        });
    }

    cancellation.cancel();
    tokio::select! {
        () = graceful.shutdown() => {}
        () = tokio::time::sleep(std::time::Duration::from_secs(30)) => {
            tracing::warn!("S3 connections exceeded graceful shutdown deadline");
        }
    }
    gateway.shutdown().await;
    multipart_maintenance.await?;
    Ok(())
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
