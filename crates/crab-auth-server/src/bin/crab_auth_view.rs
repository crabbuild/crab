use clap::{Parser, Subcommand};
use crab_auth_server::doctor::git_version;
use crab_auth_server::output::{HelperOutputPolicy, emit_json_result};
use crab_auth_server::view::{ViewOutput, materialize_view};
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
    Doctor(DoctorOutput),
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let args = Args::parse();
    let result = match args.command {
        CommandKind::Doctor => git_version().map(|version| {
            HelperOutput::Doctor(DoctorOutput {
                status: "ok".to_owned(),
                git_version: version,
            })
        }),
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

    std::process::exit(emit_json_result(HelperOutputPolicy::View, result));
}
