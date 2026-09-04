use clap::Parser;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Parser)]
#[command(about = "Serve Crab repositories and their React web application")]
struct Arguments {
    #[arg(long)]
    config: PathBuf,
    #[arg(long, help = "Check the configured listener's readiness and exit")]
    healthcheck: bool,
}

#[tokio::main]
async fn main() -> crab_http_server::Result<()> {
    tracing_subscriber::fmt()
        .with_target(false)
        .with_writer(std::io::stderr)
        .try_init()
        .map_err(|source| crab_http_server::Error::Logging { source })?;
    let arguments = Arguments::parse();
    let config = crab_http_server::Config::read(&arguments.config)?;
    if arguments.healthcheck {
        let url = format!("http://127.0.0.1:{}/readyz", config.listen.port());
        reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|source| crab_http_server::Error::Healthcheck { source })?
            .get(url)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|source| crab_http_server::Error::Healthcheck { source })?;
        return Ok(());
    }
    crab_http_server::serve(config).await
}
