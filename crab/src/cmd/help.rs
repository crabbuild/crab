//! Task-oriented root help for the Crab CLI.

use clap::Command;

use crate::core::output::OutputMode;
use crate::core::style::CliStyle;

/// User-facing top-level commands grouped by common task.
pub const COMMAND_SECTIONS: &[(&str, &[&str])] = &[
    (
        "Get started",
        &["configure", "init", "setup", "clone", "mirror", "doctor"],
    ),
    (
        "Repositories and everyday Git",
        &[
            "add", "reset", "status", "push", "pull", "ship", "download", "worktree", "import",
            "export",
        ],
    ),
    (
        "Large files and working tree",
        &[
            "track",
            "untrack",
            "why",
            "hydrate",
            "dehydrate",
            "ls-files",
            "fetch",
            "prune",
            "adopt",
            "unadopt",
            "undo",
            "migrate",
            "diff",
            "lock",
            "unlock",
            "locks",
        ],
    ),
    (
        "Workflows, data, and experiments",
        &[
            "data",
            "artifacts",
            "run",
            "repro",
            "stage",
            "freeze",
            "unfreeze",
            "exp",
            "queue",
            "workflow",
            "params",
            "metrics",
            "plots",
            "release",
        ],
    ),
    (
        "Storage and performance",
        &[
            "du", "stat", "gc", "fsck", "compact", "repack", "optimize", "tier", "metadb", "cache",
            "staging", "replica",
        ],
    ),
    (
        "Mounts and compatibility",
        &["mount", "unmount", "daemon", "lfs"],
    ),
    (
        "Cloud access and teams",
        &[
            "login",
            "logout",
            "auth",
            "organization",
            "repo",
            "member",
            "service-account",
            "audit",
            "recover",
        ],
    ),
    (
        "Configuration and tools",
        &[
            "config",
            "env",
            "logs",
            "install",
            "uninstall",
            "completions",
            "errors",
            "skills",
            "upgrade",
            "version",
        ],
    ),
];

/// Print the root help with commands organized into task-oriented sections.
pub fn print_root_help(command: &Command) {
    print!("{}", render_root_help(command));
}

/// Render the task-oriented root help.
#[must_use]
pub fn render_root_help(command: &Command) -> String {
    let style = CliStyle::resolve(OutputMode::Text);
    let width = COMMAND_SECTIONS
        .iter()
        .flat_map(|(_, commands)| commands.iter())
        .map(|command| command.len())
        .max()
        .unwrap_or(0);
    let mut output = format!(
        "{}\n\nUsage: crab [OPTIONS] [COMMAND]\n",
        style.bold("Crab — serverless Git for large files and reproducible workflows")
    );

    for (heading, names) in COMMAND_SECTIONS {
        output.push('\n');
        output.push_str(&style.bold(heading));
        output.push('\n');
        for name in *names {
            let description = command
                .get_subcommands()
                .find(|subcommand| subcommand.get_name() == *name)
                .and_then(Command::get_about)
                .map(ToString::to_string)
                .unwrap_or_default();
            output.push_str(&format!("  {name:width$}  {description}\n"));
        }
    }

    output.push_str("\nOptions:\n");
    output.push_str("      --log-level <LOG_LEVEL>  Set the log verbosity level\n");
    output.push_str("  -h, --help                   Print help\n");
    output.push_str("  -V, --version                Print version\n");
    output.push('\n');
    output.push_str(&style.dim("Run `crab <command> --help` for command options."));
    output.push('\n');
    output.push_str("Docs: https://crab.build/docs/cli\n");
    output
}
