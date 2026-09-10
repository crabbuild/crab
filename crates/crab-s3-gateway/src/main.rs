use std::path::PathBuf;

use clap::Parser;

#[derive(Debug, Parser)]
#[command(about = "Serve Crab repositories through the S3 protocol")]
struct Args {
    #[arg(long, default_value = "crab-s3-gateway.toml")]
    config: PathBuf,
    /// Initialize configured repository prefixes and exit.
    #[arg(long)]
    initialize: bool,
    /// Check the running gateway's liveness endpoint and exit.
    #[arg(long, conflicts_with_all = ["initialize", "readiness_check"])]
    healthcheck: bool,
    /// Check every configured repository through the readiness endpoint and exit.
    #[arg(long, conflicts_with_all = ["initialize", "healthcheck"])]
    readiness_check: bool,
}

#[tokio::main]
async fn main() -> Result<(), Box<crab_s3_gateway::Error>> {
    tracing_subscriber::fmt()
        .with_target(false)
        .with_writer(std::io::stderr)
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init()
        .map_err(|source| crab_s3_gateway::Error::Logging { source })?;
    let args = Args::parse();
    let config = crab_s3_gateway::Config::read(&args.config)?;
    if args.initialize {
        crab_s3_gateway::initialize(config).await?;
    } else if args.healthcheck {
        crab_s3_gateway::check_liveness(&config).await?;
    } else if args.readiness_check {
        crab_s3_gateway::check_readiness(&config).await?;
    } else {
        crab_s3_gateway::serve(config).await?;
    }
    Ok(())
}
