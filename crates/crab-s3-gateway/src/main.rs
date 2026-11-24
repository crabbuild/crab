use std::path::PathBuf;

use clap::Parser;
use crab_storage::cdn::{CdnConsumer, is_read_evidence_target, observe_service_build};
use tracing::Level;
use tracing_subscriber::{
    EnvFilter, Layer as _, filter::filter_fn, layer::SubscriberExt as _,
    util::SubscriberInitExt as _,
};

#[derive(Debug, Parser)]
#[command(about = "Serve Crab repositories through the S3 protocol")]
struct Args {
    #[arg(long, default_value = "crab-s3-gateway.toml", conflicts_with = "setup")]
    config: PathBuf,
    /// Prepare the single-bucket Docker profile from AWS_BUCKET and print its backend region.
    #[arg(long, conflicts_with_all = ["initialize", "healthcheck", "readiness_check"])]
    setup: bool,
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
    let output = tracing_subscriber::fmt::layer()
        .with_target(false)
        .with_writer(std::io::stderr)
        .with_filter(filter_fn(|metadata| {
            !is_read_evidence_target(metadata.target())
                && dependency_event_is_safe(metadata.target(), metadata.level())
        }));
    let evidence = tracing_subscriber::fmt::layer()
        .json()
        .with_current_span(false)
        .with_span_list(false)
        .with_writer(std::io::stderr)
        .with_filter(filter_fn(|metadata| {
            is_read_evidence_target(metadata.target())
        }));
    tracing_subscriber::registry()
        .with(output)
        .with(evidence)
        .with(EnvFilter::from_default_env())
        .try_init()
        .map_err(|source| crab_s3_gateway::Error::Logging {
            source: Box::new(source),
        })?;
    let args = Args::parse();
    if args.setup {
        println!("{}", crab_s3_gateway::setup_container().await?);
        return Ok(());
    }
    let config = crab_s3_gateway::Config::read(&args.config)?;
    if args.initialize {
        crab_s3_gateway::initialize(config).await?;
    } else if args.healthcheck {
        crab_s3_gateway::check_liveness(&config).await?;
    } else if args.readiness_check {
        crab_s3_gateway::check_readiness(&config).await?;
    } else {
        observe_service_build(
            CdnConsumer::S3Gateway,
            env!("CRAB_SERVICE_SOURCE_SHA"),
            env!("CRAB_SERVICE_TARGET"),
            env!("CRAB_SERVICE_PROFILE"),
            env!("CRAB_SERVICE_FEATURES"),
        );
        crab_s3_gateway::serve(config).await?;
    }
    Ok(())
}

fn dependency_event_is_safe(target: &str, level: &Level) -> bool {
    if target != "s3s" && !target.starts_with("s3s::") {
        return true;
    }

    // s3s debug events include complete signed requests and signature material.
    // Its XML decoder also attaches raw malformed bodies to error events.
    let raw_body_logger = target == "s3s::http::de" || target.starts_with("s3s::http::de::");
    !raw_body_logger && matches!(*level, Level::ERROR | Level::WARN | Level::INFO)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setup_is_exclusive_with_explicit_runtime_actions() {
        assert!(Args::try_parse_from(["gateway", "--setup"]).is_ok());
        for flag in [
            "--initialize",
            "--healthcheck",
            "--readiness-check",
            "--config",
        ] {
            let mut args = vec!["gateway", "--setup", flag];
            if flag == "--config" {
                args.push("gateway.toml");
            }
            assert!(Args::try_parse_from(args).is_err(), "{flag}");
        }
    }

    #[test]
    fn dependency_filter_cannot_expose_signed_requests_or_malformed_bodies() {
        assert!(!dependency_event_is_safe("s3s::service", &Level::DEBUG));
        assert!(!dependency_event_is_safe(
            "s3s::ops::signature",
            &Level::TRACE
        ));
        assert!(!dependency_event_is_safe("s3s::http::de", &Level::ERROR));
        assert!(dependency_event_is_safe("s3s::service", &Level::ERROR));
        assert!(dependency_event_is_safe(
            "crab_s3_gateway::server",
            &Level::DEBUG
        ));
        assert!(dependency_event_is_safe("s3store", &Level::DEBUG));
    }
}
