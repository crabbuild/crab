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
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init()?;
    let args = Args::parse();
    let config = crab_s3_gateway::Config::read(&args.config)?;
    if args.initialize {
        crab_s3_gateway::initialize(config).await?;
    } else {
        crab_s3_gateway::serve(config).await?;
    }
    Ok(())
}
