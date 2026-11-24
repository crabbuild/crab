use clap::{Parser, Subcommand};
use crab_auth_server::doctor::git_version;
use crab_auth_server::output::{HelperOutputPolicy, emit_json_result};
use crab_auth_server::view::{
    ViewOutput, ViewRetentionRequest, ViewRetentionResponse, materialize_view, view_retention,
};
use serde::Serialize;

#[derive(Parser)]
#[command(name = "crab-auth-view", version)]
struct Args {
    #[command(subcommand)]
    command: CommandKind,
}

#[derive(Subcommand)]
enum CommandKind {
    Doctor,
    Retention,
    Materialize {
        #[arg(long)]
        repo_url: String,
        #[arg(long)]
        provider: String,
        #[arg(long)]
        scope_hash: String,
        #[arg(long = "read-path")]
        read_paths: Vec<String>,
        #[arg(long = "deny-path")]
        deny_paths: Vec<String>,
    },
}

#[derive(Serialize)]
struct DoctorOutput {
    status: String,
    git_version: String,
}

#[derive(Serialize)]
#[serde(untagged)]
enum HelperOutput {
    View(ViewOutput),
    Retention(ViewRetentionResponse),
    Doctor(DoctorOutput),
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let args = Args::parse();
    let policy = if matches!(args.command, CommandKind::Retention) {
        HelperOutputPolicy::ViewRetention
    } else {
        HelperOutputPolicy::View
    };
    let result = match args.command {
        CommandKind::Doctor => git_version().map(|version| {
            HelperOutput::Doctor(DoctorOutput {
                status: "ok".to_owned(),
                git_version: version,
            })
        }),
        CommandKind::Retention => match ViewRetentionRequest::read(std::io::stdin().lock()) {
            Ok(request) => view_retention(request).await.map(HelperOutput::Retention),
            Err(error) => Err(error),
        },
        CommandKind::Materialize {
            repo_url,
            provider,
            scope_hash,
            read_paths,
            deny_paths,
        } => materialize_view(&repo_url, &provider, &scope_hash, &read_paths, &deny_paths)
            .await
            .map(HelperOutput::View),
    };

    std::process::exit(emit_json_result(policy, result));
}
